use rust_i18n::t;

mod audio_preview;
mod caret_hints;
mod code_theme;
mod documents;
mod edit_history;
mod editor_git;
mod file_drop;
mod file_icons;
mod file_tabs;
mod file_tree;
mod folder_icons;
mod git_history;
mod git_panel;
mod image_preview;
mod image_tab;
mod inlay_hints;
mod language_actions;
mod language_service;
mod line_alignment;
mod locator;
mod markdown_editing;
mod markdown_links;
mod repo_sync;
mod repository_ui;
mod session;
mod settings;
mod source_history;
mod standalone;
mod system_files;
mod terminal;
mod toast;
mod tree_motion;
mod tree_transfer;
mod window_bounds;

pub use window_bounds::{repository_window_bounds, standalone_window_bounds};

use file_icons::file_type_icon;
use git_panel::GitPanel;
use language_service::{LanguageService, ServerKey, ServerProject};
use repository_ui::RepositoryUi;
use settings::SettingsWindow;

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme, Colorize as _, Disableable as _, Icon, Placement, Selectable as _, Sizable as _,
    StyledExt, Theme, ThemeMode, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Editor, EditorState, Enter, IndentInline, InputEvent, Rope, Search},
    menu::{DropdownMenu, PopupMenuItem},
    resizable::HandleEdge,
    scroll::{Scrollbar, ScrollbarHandle, thin_scrollbar},
    v_flex, window_corner_radii, window_paddings,
};
use gpui_kit_assets::IconName;

use crate::{
    config::{CodeContrast, CodeTheme, ConfigFile, Lsp, Theme as ThemePreference},
    docs::{self, Destination, DocReader, DocumentTree},
    image_viewer::Preview,
    lsp::{self, EditorLsp},
    paths,
    repository::{Document, Entry, Location, Repository, Store, WindowGeometry, WindowSlot},
    session::{FilePosition, WorktreeSession},
    tabs::TabSession,
};

actions!(
    workspace,
    [
        OpenRepository,
        CloseRepositoryPicker,
        NextRepositoryPath,
        PreviousRepositoryPath,
        CompleteRepositoryPath,
        FindInDocument,
        OpenLocator,
        FindReferences,
        FormatDocument,
        SaveFile,
        ToggleWrap,
        CloseTab,
        NextTab,
        PreviousTab,
        ToggleTerminal,
        ToggleSettings,
        CloseSettings
    ]
);

#[derive(Clone)]
pub(crate) struct PaneDrag {
    pub(crate) edge: HandleEdge,
    offset: Rc<Cell<Pixels>>,
}

impl PaneDrag {
    pub(crate) fn offset(&self) -> Pixels {
        self.offset.get()
    }
}

impl Render for PaneDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

const FILE_TOOLBAR_HEIGHT: Pixels = px(36.);
const PANE_RESIZE_GUTTER: Pixels = px(5.);
const PANE_RESIZE_HIT_WIDTH: Pixels = px(4.);
const PANE_SHADOW_OPACITY: f32 = 0.09;
const DARK_PANE_DIVIDER: u32 = 0x3d4048;
const RECENT_REPOSITORY_LIMIT: usize = 3;
const EDITOR_JUMP_CONTEXT_LINES: usize = 5;

fn pane_resize_handle<T: 'static, E: 'static + Render>(
    id: &'static str,
    axis: Axis,
    draw_divider: bool,
) -> gpui_base::ResizeHandle<T, E> {
    gpui_base::resize_handle(id, axis)
        .with_hover_delay(Duration::from_millis(120))
        .with_scroll_passthrough()
        .with_appearance(Rc::new(move |handle, window, cx| {
            use gpui_base::{ResizeHandleState, Transition, transition};

            let (length, opacity) = match handle.state() {
                ResizeHandleState::Idle => (px(0.), 0.),
                ResizeHandleState::Hovered => (px(32.), 0.7),
                ResizeHandleState::Pressed => (px(44.), 0.9),
                ResizeHandleState::Dragging => (px(52.), 1.),
            };
            let theme = cx.theme();
            let (border, accent, radius) = (theme.border, theme.accent, theme.radius_full());
            let motion = theme.motion_tokens();
            let policy = Transition::new(motion.duration_fast).easing(motion.easing_move.clone());
            let length = transition("pane-thumb-length", length, policy.clone(), window, cx);
            let opacity =
                transition("pane-thumb-opacity", opacity, policy, window, cx).clamp(0., 1.);
            let thickness = px(4.);
            let offset = (px(1.) - thickness) * 0.5;

            Some(
                div()
                    .flex_none()
                    .flex()
                    .when(draw_divider, |line| line.bg(border))
                    .map(|line| match axis {
                        Axis::Horizontal => line.w(px(1.)).h_full().items_center(),
                        Axis::Vertical => line.h(px(1.)).w_full().items_start().justify_center(),
                    })
                    .when(length > px(0.5), |line| {
                        // Draw above both panes and their borders, below popups.
                        line.child(deferred(
                            div()
                                .flex_none()
                                .rounded(radius)
                                .bg(accent)
                                .opacity(opacity)
                                .map(|pill| match axis {
                                    Axis::Horizontal => pill.w(thickness).h(length).ml(offset),
                                    Axis::Vertical => pill.h(thickness).w(length).mt(offset),
                                }),
                        ))
                    })
                    .into_any_element(),
            )
        }))
}

#[track_caller]
pub(crate) fn pane_scrollbar<H: ScrollbarHandle + Clone>(handle: &H) -> Scrollbar {
    thin_scrollbar(handle).styles(|styles| styles.thumb(|thumb| thumb.inset(px(4.))))
}

pub(crate) fn panel_resize(id: &'static str, edge: HandleEdge, top: Pixels) -> impl IntoElement {
    div()
        .absolute()
        .top(top)
        .bottom_0()
        .w(PANE_RESIZE_HIT_WIDTH)
        .map(|band| match edge {
            HandleEdge::Leading => band.left_0(),
            HandleEdge::Trailing => band.right_0(),
        })
        .child(
            pane_resize_handle(id, Axis::Horizontal, true)
                .inside(edge)
                .with_hit_padding(
                    PANE_RESIZE_HIT_WIDTH - px(1.),
                    PANE_RESIZE_HIT_WIDTH - px(1.),
                )
                .on_drag_from_press(
                    PaneDrag {
                        edge,
                        offset: Rc::new(Cell::new(px(0.))),
                    },
                    |drag, position, _, cx| {
                        let offset = match drag.edge {
                            HandleEdge::Leading => position.x,
                            HandleEdge::Trailing => position.x - PANE_RESIZE_HIT_WIDTH,
                        };
                        // GPUI retains the payload separately from the drag view.
                        drag.offset.set(offset);
                        cx.new(|_| drag.clone())
                    },
                ),
        )
}

fn pane_edge(edge: HandleEdge, top: Pixels, mode: ThemeMode) -> impl IntoElement {
    let line = px(1.);
    let (inset, width, background) = if mode.is_dark() {
        (px(0.), line, rgb(DARK_PANE_DIVIDER).into())
    } else {
        let black = Hsla::black();
        let (start, end) = match edge {
            HandleEdge::Leading => (black.opacity(PANE_SHADOW_OPACITY), black.opacity(0.)),
            HandleEdge::Trailing => (black.opacity(0.), black.opacity(PANE_SHADOW_OPACITY)),
        };
        (
            line,
            PANE_RESIZE_GUTTER - line,
            linear_gradient(
                90.,
                linear_color_stop(start, 0.),
                linear_color_stop(end, 1.),
            ),
        )
    };
    // Paint inside the resize band without inserting a hitbox over the handle.
    let edge_paint = canvas(
        |_, _, _| (),
        move |bounds, (), window, _| window.paint_quad(fill(bounds, background)),
    )
    .absolute()
    .top(top)
    .bottom_0()
    .w(width);
    match edge {
        HandleEdge::Leading => edge_paint.left(inset),
        HandleEdge::Trailing => edge_paint.right(inset),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) bg: Hsla,
    pub(crate) elevated: Hsla,
    titlebar: Hsla,
    nav_track: Hsla,
    pub(crate) soft: Hsla,
    pub(crate) hover: Hsla,
    pub(crate) border: Hsla,
    pub(crate) surface: Hsla,
    pub(crate) ink: Hsla,
    pub(crate) muted: Hsla,
    pub(crate) accent: Hsla,
    pub(crate) warning: Hsla,
    pub(crate) danger: Hsla,
    /// Border color for unchecked custom controls such as checkboxes, which have transparent backgrounds.
    pub(crate) control: Hsla,
}

fn editor_scroll_viewport(state: &Entity<EditorState>, background: Hsla, enabled: bool) -> Div {
    div()
        .relative()
        .when(enabled && background.is_opaque(), |viewport| {
            let state = state.clone();
            viewport
                .partial_scroll(move |cx| state.read(cx).scroll_offset().y)
                .child(scroll_viewport_background(background))
        })
}

pub(crate) fn scroll_viewport_background(background: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            let radii = window_corner_radii(window, cx);
            let padding = window_paddings(window);
            let viewport = window.viewport_size();
            let frame_right = viewport.width - padding.right;
            let frame_bottom = viewport.height - padding.bottom;
            let near_bottom = frame_bottom - bounds.bottom();
            let left = bounds.left() - padding.left;
            let right = frame_right - bounds.right();
            let frame_radius = cx.theme().window_radius;
            let round_left =
                radii.bottom_left > px(0.) && left < frame_radius && near_bottom < frame_radius;
            let round_right =
                radii.bottom_right > px(0.) && right < frame_radius && near_bottom < frame_radius;
            if !round_left && !round_right {
                window.paint_quad(fill(bounds, background));
                return;
            }

            // Align the arcs with the frame, even when the viewport only covers part of a corner.
            let border_width = frame_radius - radii.bottom_left.max(radii.bottom_right);
            let mut origin = bounds.origin;
            let mut end = bounds.bottom_right();
            end.y = frame_bottom - border_width;
            let mut corners = Corners::default();
            if round_left {
                origin.x = padding.left + border_width;
                corners.bottom_left = radii.bottom_left;
            }
            if round_right {
                end.x = frame_right - border_width;
                corners.bottom_right = radii.bottom_right;
            }
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                window.paint_quad(PaintQuad {
                    corner_radii: corners,
                    ..fill(Bounds::from_corners(origin, end), background)
                });
            });
        },
    )
    .absolute()
    .inset_0()
}

impl Palette {
    pub(crate) fn for_mode(mode: ThemeMode) -> Self {
        let [
            bg,
            elevated,
            soft,
            hover,
            border,
            surface,
            ink,
            muted,
            accent,
            warning,
            danger,
            control,
        ] = match mode {
            ThemeMode::Dark => [
                0x141518, 0x1c1d22, 0x232428, 0x2b2c32, 0x2e2f36, 0x3b414e, 0xdcdde2, 0xa3a5ad,
                0x8ea2ff, 0xd3a95f, 0xe3837a, 0x757983,
            ],
            ThemeMode::Light => [
                0xffffff, 0xffffff, 0xf5f6f9, 0xeceff5, 0xe4e4df, 0xd7dce8, 0x303540, 0x697386,
                0x4f6bed, 0xa7741f, 0xc0453b, 0x878d9a,
            ],
        }
        .map(|color| rgb(color).into());
        Self {
            bg,
            elevated,
            titlebar: if mode == ThemeMode::Light {
                soft
            } else {
                elevated
            },
            nav_track: rgb(if mode == ThemeMode::Light {
                0xe8ebf1
            } else {
                0x15161a
            })
            .into(),
            soft,
            hover,
            border,
            surface,
            ink,
            muted,
            accent,
            warning,
            danger,
            control,
        }
    }
}

pub fn apply_theme(mode: ThemeMode, cx: &mut App) {
    apply_theme_with_fonts(
        mode,
        px(crate::config::Appearance::default().ui_font_size as f32),
        None,
        None,
        None,
        CodeTheme::default(),
        CodeContrast::Enhanced,
        cx,
    );
}

/// The monospace font configured for the code editor, when it is installed.
struct CodeFontOverride(Option<SharedString>);

impl Global for CodeFontOverride {}

/// The family every code surface outside the code editor draws in: code blocks
/// and inline code in documents, code snippets in the UI, and the markdown in
/// the editor's LSP popovers.
pub(crate) fn ui_code_font(cx: &App) -> SharedString {
    cx.try_global::<CodeFontOverride>()
        .and_then(|font| font.0.clone())
        .unwrap_or_else(|| cx.theme().mono_font_family.clone())
}

fn apply_theme_with_fonts(
    mode: ThemeMode,
    ui_font_size: Pixels,
    ui_font: Option<SharedString>,
    code_font: Option<SharedString>,
    fallbacks: Option<FontFallbacks>,
    code_theme: CodeTheme,
    code_contrast: CodeContrast,
    cx: &mut App,
) {
    Theme::change(mode, None, cx);
    let palette = Palette::for_mode(mode);
    let mono_font = kde_monospace().filter(|font| cx.text_system().all_font_names().contains(font));
    cx.set_global(CodeFontOverride(code_font));
    Theme::update(cx, |theme| {
        let mut highlight = code_theme::highlight_theme(
            code_theme,
            code_contrast,
            mode,
            &[palette.bg, palette.elevated, palette.soft],
        );
        code_theme::apply_diff_contrast(
            std::sync::Arc::make_mut(&mut highlight),
            code_theme,
            code_contrast,
            mode,
        );
        theme.highlight_theme = highlight;
        theme.window_radius = px(8.);
        theme.font_size = ui_font_size;
        theme.control_font_size = ui_font_size;
        theme.mono_font_size = px(13.5);
        theme.font_family = ui_font.unwrap_or_else(|| {
            let config = if mode.is_dark() {
                &theme.dark_theme
            } else {
                &theme.light_theme
            };
            config
                .font_family
                .clone()
                .unwrap_or_else(|| ".SystemUIFont".into())
        });
        theme.font_fallbacks = fallbacks;
        if let Some(font) = mono_font {
            theme.mono_font_family = font.into();
        }
        theme.colors.background = palette.bg;
        theme.colors.foreground = palette.ink;
        theme.colors.border = palette.border;
        theme.colors.muted = palette.soft;
        theme.colors.muted_foreground = palette.muted;
        theme.colors.title_bar = palette.elevated;
        theme.colors.title_bar_border = palette.bg;
        theme.colors.accent = palette.accent;
        theme.colors.secondary_foreground = palette.muted;
        theme.colors.primary = palette.accent;
        theme.colors.button_primary = palette.accent;
        theme.colors.button_primary_hover = palette.accent.mix_oklab(palette.ink, 0.94);
        theme.colors.button_primary_active = palette.accent.mix_oklab(palette.ink, 0.88);
        theme.colors.button_primary_foreground = if mode.is_dark() {
            palette.bg
        } else {
            Hsla::white()
        };
        theme.colors.secondary_hover = palette.hover;
        theme.colors.secondary_active = palette.hover;
        theme.colors.button_active = palette.hover.mix_oklab(palette.ink, 0.90);
        theme.colors.button_secondary = palette.hover;
        theme.colors.button_secondary_foreground = palette.ink;
        theme.colors.button_secondary_hover = palette.hover.mix_oklab(palette.ink, 0.94);
        theme.colors.button_secondary_active = theme.colors.button_active;
        theme.colors.ring = palette.accent;
        theme.focus_ring = false;
        theme.colors.link = palette.accent;
        theme.colors.caret = palette.accent;
        theme.colors.selection = palette.accent.opacity(0.23);
        theme.colors.popover = palette.elevated;
        theme.colors.popover_foreground = palette.ink;
        theme.radius = px(8.);
        theme.notification.placement = toast::PLACEMENT;
    });
}

fn kde_monospace() -> Option<String> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    let settings = fs::read_to_string(config.join("kdeglobals")).ok()?;
    settings
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "[General]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == "fixed").then(|| {
                value
                    .split(',')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            })
        })
}

enum FileOpen {
    Select {
        preview: bool,
        position: Option<SourcePosition>,
    },
    Locate(Option<(std::ops::Range<usize>, [u8; 32])>),
    History {
        index: usize,
        position: Option<FilePosition>,
    },
    Startup,
    Restore,
}

enum SourcePosition {
    Lsp(lsp_types::Position),
    MarkdownFragment(String),
    Diff(DiffSource),
}

struct DiffSource {
    row: usize,
    snapshot: Option<Task<Option<Rope>>>,
}

fn reveal_editor_range(
    editor: &mut EditorState,
    range: std::ops::Range<usize>,
    leading_block: Option<usize>,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    use gpui_component::input::RopeExt as _;
    editor.unfold_at(editor.text().offset_to_position(range.start), cx);
    if !range.is_empty() {
        editor.unfold_at(editor.text().offset_to_position(range.end), cx);
    }
    editor.set_selected_range(range, cx);
    editor.focus(window, cx);
    editor.scroll_to_caret_with_context(EDITOR_JUMP_CONTEXT_LINES, leading_block, cx);
}

struct DocumentOpen {
    path: PathBuf,
    fragment: Option<String>,
    position: Option<(std::ops::Range<usize>, [u8; 32])>,
}

type ReviewBufferCallback =
    Box<dyn FnOnce(&mut Workspace, Result<()>, &mut Window, &mut Context<Workspace>)>;

#[derive(Clone)]
struct FileBuffer {
    document: Document,
    text: Rope,
    saved: Rope,
}

impl FileBuffer {
    fn new(document: Document) -> Self {
        let text = Rope::from(document.text.as_str());
        let saved = if document.restored {
            Rope::from(document.saved_text.as_str())
        } else {
            text.clone()
        };
        Self {
            document,
            text,
            saved,
        }
    }
}

#[derive(Default)]
struct DraftWrite {
    revision: u64,
    result: Option<Result<bool>>,
}

struct OpenFile {
    path: PathBuf,
    document: Document,
    editor: Entity<EditorState>,
    committed: Rope,
    saved: Rope,
    draft_write: Arc<Mutex<DraftWrite>>,
    revision: u64,
    queued_revision: Option<u64>,
    _changes: Subscription,
    _editor_observer: Subscription,
    _input_edits: Subscription,
    newline: Option<crate::lsp::Newline>,
    newline_task: Task<()>,
    server_project: Option<ServerProject>,
    language_server: Option<Rc<EditorLsp>>,
    _lsp_refresh: Task<()>,
    inlay_hints: inlay_hints::InlayHints,
    caret_hints: caret_hints::CaretHints,
    dirty: bool,
    pending_draft: bool,
    unconfirmed_save: Option<Rope>,
    missing: bool,
    sync_revision: u64,
    wrap: bool,
    git: editor_git::EditorGit,
    restore_position: Option<FilePosition>,
}

impl OpenFile {
    fn is_dirty(&self) -> bool {
        self.dirty || self.pending_draft
    }

    fn flush_draft(&mut self, repo: &Repository, store: &Store) -> Result<()> {
        if !self.pending_draft || self.document.conflict {
            return Ok(());
        }
        let mut write = self.draft_write.lock().unwrap();
        let dirty = match write
            .result
            .take()
            .filter(|_| write.revision == self.revision)
        {
            Some(Ok(dirty)) => dirty,
            _ => {
                let text = self.committed.to_string();
                let dirty = self.committed != self.saved;
                self.document
                    .persist_draft(repo, &self.path, store, &text, dirty)?;
                dirty
            }
        };
        write.revision = self.revision;
        self.queued_revision = None;
        self.dirty = dirty;
        self.pending_draft = false;
        Ok(())
    }

    fn apply_wrap(&mut self, wrap: bool, window: &mut Window, cx: &mut App) {
        if self.wrap != wrap {
            self.wrap = wrap;
            self.editor
                .update(cx, |editor, cx| editor.set_soft_wrap(wrap, window, cx));
        }
    }
}

pub use crate::session::WorkspaceView;

#[derive(Clone)]
struct TreeRow {
    entry: Entry,
    depth: usize,
    expanded: bool,
    dimmed: bool,
    guides: file_tree::TreeGuides,
}

pub struct Workspace {
    store: Arc<Store>,
    config: ConfigFile,
    ai: crate::ai::Service,
    settings: Option<SettingsWindow>,
    startup_lsp: Lsp,
    editor_font: Option<SharedString>,
    repository: Option<Arc<Repository>>,
    // Refresh the tree without replacing the active repository's identity for in-flight work.
    repository_snapshot: Option<Arc<Repository>>,
    recent_ssh_repositories: VecDeque<Arc<Repository>>,
    tree_status_task: Task<()>,
    tree_load_pending: BTreeSet<PathBuf>,
    tree_reveal: Option<PathBuf>,
    tree_selected: Option<PathBuf>,
    docs_tree_task: Task<()>,
    docs_tree_seeded: bool,
    docs_scan_pending: bool,
    docs_tree_complete: bool,
    recent_docs: VecDeque<documents::CachedDocs>,
    repository_watch_pending: bool,
    docs_tree_loading: bool,
    docs_tree_dirty: bool,
    workspace_refresh: Task<()>,
    workspace_refresh_pending: crate::git::watch::RepoChanges,
    workspace_refreshing: bool,
    tree_status: Rc<file_tree::TreeStatus>,
    tree_edit: Option<file_tree::TreeEdit>,
    tree_busy: bool,
    tree_focus: FocusHandle,
    tree_keyboard_path: PathBuf,
    tree_clipboard: Option<tree_transfer::TreeClipboard>,
    system_files: system_files::SystemFiles,
    standalone: Option<PathBuf>,
    docs_tree_width: Pixels,
    editor_tree_width: Pixels,
    pub(crate) outline_width: Pixels,
    pub(crate) outline_visibility: Option<bool>,
    language_servers: HashMap<ServerKey, LanguageService>,
    lsp_panel_open: bool,
    language_action_groups: BTreeMap<u64, Vec<PathBuf>>,
    language_rename: Task<()>,
    language_format: Task<()>,
    expanded: BTreeSet<PathBuf>,
    rows: Rc<Vec<TreeRow>>,
    tree_scroll: UniformListScrollHandle,
    tree_motion: std::cell::RefCell<tree_motion::TreeMotion<TreeRow>>,
    tabs: TabSession,
    source_caret: Option<(u64, crate::session::SourceLocation)>,
    source_click: Option<(u64, crate::session::SourceLocation)>,
    files: HashMap<PathBuf, OpenFile>,
    tab_scroll: ScrollHandle,
    tab_viewport_width: Pixels,
    tabs_overflow: bool,
    open_files_list: bool,
    open_files_scroll: UniformListScrollHandle,
    close_queue: VecDeque<PathBuf>,
    confirming_close: bool,
    closing_standalone_window: bool,
    view: WorkspaceView,
    git: Option<Entity<GitPanel>>,
    git_edit: HashSet<PathBuf>,
    git_history: git_history::GitHistory,
    git_watch: Task<()>,
    docs_tree: Rc<DocumentTree>,
    docs_expanded: BTreeSet<PathBuf>,
    doc: Option<Entity<DocReader>>,
    doc_loading: Option<DocumentOpen>,
    doc_error: Option<String>,
    doc_task: Task<()>,
    doc_sync: Task<()>,
    open_documents_list: bool,
    open_documents_scroll: UniformListScrollHandle,
    document_list_selection: usize,
    document_list_trigger: FocusHandle,
    document_list_focus: FocusHandle,
    focus: FocusHandle,
    repository_ui: RepositoryUi,
    locator: locator::Locator,
    terminals: terminal::Terminals,
    wrap: bool,
    file_task: Task<()>,
    image_preview: Option<Preview>,
    image_preview_focus: FocusHandle,
    image_preview_return_focus: Option<FocusHandle>,
    images: HashMap<PathBuf, image_tab::ImageTab>,
    audio_preview: audio_preview::AudioPreview,
    review_loads: HashMap<PathBuf, Vec<ReviewBufferCallback>>,
    saving: BTreeSet<PathBuf>,
    connection_watch: Task<()>,
    repository_watch: Task<()>,
    repository_watch_active: bool,
    loading_file: Option<(PathBuf, FileOpen)>,
    draft_task: Task<()>,
    session: WorktreeSession,
    session_write: Arc<Mutex<u64>>,
    session_revision: u64,
    startup_view: Option<WorkspaceView>,
    saved_window: Option<(WindowSlot, WindowGeometry)>,
    pending_window: Option<(WindowSlot, WindowGeometry)>,
    window_write: Task<()>,
}

impl Workspace {
    pub fn new(
        store: Arc<Store>,
        view: WorkspaceView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_config(store, ConfigFile::from_environment(), view, window, cx)
    }

    pub fn with_config(
        store: Arc<Store>,
        config: ConfigFile,
        view: WorkspaceView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_startup_view(store, config, Some(view), window, cx)
    }

    pub fn with_startup_view(
        store: Arc<Store>,
        config: ConfigFile,
        view: Option<WorkspaceView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_window(store, config, view, None, window, cx)
    }

    fn with_window(
        store: Arc<Store>,
        config: ConfigFile,
        view: Option<WorkspaceView>,
        standalone: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository_mode = standalone.is_none();
        Self::bind_git_history_keys(cx);
        Self::bind_source_navigation_keys(cx);
        Self::bind_caret_hint_keys(cx);
        Self::bind_tree_keys(cx);
        #[cfg(target_os = "linux")]
        if repository_mode && matches!(cx.compositor_name(), "Wayland" | "X11") {
            let probe = cx.background_spawn(async { system_files::SystemFiles::load() });
            cx.spawn(async move |workspace, cx| {
                let actions = probe.await;
                _ = workspace.update(cx, |workspace, cx| {
                    workspace.system_files = actions;
                    cx.notify();
                });
            })
            .detach();
        }
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        cx.on_app_quit(|this, cx| {
            this.audio_preview.stop();
            if let Err(error) = this.flush_session(cx) {
                eprintln!("{error:#}");
            }
            this.shutdown_terminals(cx);
            async {}
        })
        .detach();
        let weak = cx.weak_entity();
        window
            .observe_window_appearance(move |window, cx| {
                _ = weak.update(cx, |this, cx| {
                    if this.config.current().appearance.theme == ThemePreference::System {
                        this.apply_appearance(window, cx);
                    }
                });
            })
            .detach();
        let wrap = config.current().editor.soft_wrap;
        let editor_font = Self::configured_editor_font(config.current(), cx);
        let startup_lsp = config.current().lsp.clone();
        let ai = store.ai(&config);
        let mut workspace = Self {
            ai,
            locator: locator::Locator::new(),
            repository_ui: RepositoryUi::new(repository_mode.then_some(store.as_ref()), window, cx),
            terminals: terminal::Terminals::new(repository_mode.then_some(store.as_ref())),
            store,
            config,
            settings: None,
            startup_lsp,
            editor_font,
            repository: None,
            repository_snapshot: None,
            recent_ssh_repositories: VecDeque::new(),
            tree_status_task: Task::ready(()),
            tree_load_pending: BTreeSet::new(),
            tree_reveal: None,
            tree_selected: None,
            docs_tree_task: Task::ready(()),
            docs_tree_seeded: false,
            docs_scan_pending: false,
            docs_tree_complete: false,
            recent_docs: VecDeque::new(),
            repository_watch_pending: false,
            docs_tree_loading: false,
            docs_tree_dirty: true,
            workspace_refresh: Task::ready(()),
            workspace_refresh_pending: Default::default(),
            workspace_refreshing: false,
            tree_status: Rc::default(),
            tree_edit: None,
            tree_busy: false,
            tree_focus: cx.focus_handle(),
            tree_keyboard_path: PathBuf::new(),
            tree_clipboard: None,
            system_files: Default::default(),
            standalone,
            docs_tree_width: px(240.),
            editor_tree_width: px(240.),
            outline_width: px(195.),
            outline_visibility: None,
            language_servers: HashMap::new(),
            lsp_panel_open: false,
            language_action_groups: BTreeMap::new(),
            language_rename: Task::ready(()),
            language_format: Task::ready(()),
            expanded: BTreeSet::new(),
            rows: Rc::default(),
            tree_scroll: UniformListScrollHandle::new(),
            tree_motion: Default::default(),
            tabs: TabSession::default(),
            source_caret: None,
            source_click: None,
            files: HashMap::new(),
            tab_scroll: ScrollHandle::new(),
            tab_viewport_width: px(0.),
            tabs_overflow: false,
            open_files_list: false,
            open_files_scroll: UniformListScrollHandle::new(),
            close_queue: VecDeque::new(),
            confirming_close: false,
            closing_standalone_window: false,
            view: view.unwrap_or_default(),
            git: None,
            git_edit: HashSet::new(),
            git_history: Default::default(),
            git_watch: Task::ready(()),
            docs_tree: Rc::default(),
            docs_expanded: BTreeSet::new(),
            doc: None,
            doc_loading: None,
            doc_error: None,
            doc_task: Task::ready(()),
            doc_sync: Task::ready(()),
            open_documents_list: false,
            open_documents_scroll: UniformListScrollHandle::new(),
            document_list_selection: 0,
            document_list_trigger: cx.focus_handle(),
            document_list_focus: cx.focus_handle(),
            focus,
            wrap,
            file_task: Task::ready(()),
            image_preview: None,
            image_preview_focus: cx.focus_handle(),
            image_preview_return_focus: None,
            images: HashMap::new(),
            audio_preview: audio_preview::AudioPreview::default(),
            review_loads: HashMap::new(),
            saving: BTreeSet::new(),
            connection_watch: Task::ready(()),
            repository_watch: Task::ready(()),
            repository_watch_active: false,
            loading_file: None,
            draft_task: Task::ready(()),
            session: WorktreeSession::default(),
            session_write: Arc::default(),
            session_revision: 0,
            startup_view: view,
            saved_window: None,
            pending_window: None,
            window_write: Task::ready(()),
        };
        workspace.apply_appearance(window, cx);
        let mut mode = cx.theme().mode;
        cx.observe_global::<Theme>(move |workspace, cx| {
            let next = cx.theme().mode;
            if mode != next {
                mode = next;
                workspace.refresh_expanded_editor_hunks(cx);
            }
        })
        .detach();
        workspace.start_session_writer(cx);
        workspace.observe_window_geometry(window, cx);
        workspace
    }

    pub fn open_repository(
        &mut self,
        location: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_repository_with_ssh(location, None, window, cx);
    }

    fn open_repository_with_ssh(
        &mut self,
        location: Location,
        ssh: Option<crate::remote::SshSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming_close || self.repository_ui.opening.is_some() || !self.saving.is_empty()
        {
            return;
        }
        self.repository_ui.error = None;
        self.repository_ui.menu_error = None;
        let from_menu = self.repository_ui.menu_open;
        self.audio_preview.stop();
        self.repository_ui.opening = Some(location.root.clone());
        let store = self.store.clone();
        let current = self.repository.clone();
        let cached = self
            .recent_ssh_repositories
            .iter()
            .find(|repo| repo.root == location.root)
            .cloned();
        let cached_docs: BTreeSet<_> = self
            .recent_docs
            .iter()
            .map(|cached| cached.key.clone())
            .collect();
        let startup_view = self.startup_view;
        let view = self.view;
        let task = cx.background_spawn(async move {
            let prepare_docs =
                |repo: &Repository, session: &WorktreeSession, file: Option<&Path>| {
                    let target_view = startup_view.unwrap_or_else(|| {
                        if file.is_some() {
                            WorkspaceView::Editor
                        } else if current
                            .as_ref()
                            .is_some_and(|current| current.common_dir == repo.common_dir)
                        {
                            view
                        } else {
                            session.view.unwrap_or_default()
                        }
                    });
                    (target_view == WorkspaceView::Docs
                        && !cached_docs.contains(repo.storage_key())
                        && repo.supports_document_batches())
                    .then(|| {
                        repo.fast_document_paths()
                            .ok()
                            .map(DocumentTree::from_paths)
                    })
                    .flatten()
                };
            if let Some(repo) =
                cached.filter(|repo| repo.ssh.as_ref().is_some_and(|ssh| ssh.failure().is_none()))
            {
                let session = store.session(&repo)?;
                if repo.ssh.as_ref().is_some_and(|ssh| ssh.failure().is_none()) {
                    let docs = prepare_docs(&repo, &session, location.file.as_deref());
                    return Ok(Some(((*repo).clone(), session, location.file, docs, true)));
                }
            }
            let repo = if let Some(target) = crate::remote::SshTarget::parse(&location.root)? {
                match ssh {
                    Some(ssh)
                        if ssh.target().authority() == target.authority()
                            && ssh.failure().is_none() =>
                    {
                        Repository::open_ssh(ssh, target.path)?
                    }
                    _ => Repository::open(&location.root)?,
                }
            } else {
                let root = crate::repository::repository_root(&location.root)?;
                if current
                    .as_ref()
                    .is_some_and(|current| current.ssh.is_none() && current.root == root)
                    && location.file.is_none()
                {
                    return Ok::<_, anyhow::Error>(None);
                }
                Repository::open_root(root)?
            };
            repo.language_resources();
            let session = store.session(&repo)?;
            let docs = prepare_docs(&repo, &session, location.file.as_deref());
            Ok(Some((repo, session, location.file, docs, false)))
        });
        self.repository_ui.open_task = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, move |this, window, cx| {
                this.repository_ui.opening = None;
                let result = result.and_then(|target| {
                    if let Some((repo, session, file, docs, reused)) = target {
                        this.flush_drafts(cx)?;
                        this.flush_session(cx)?;
                        if this.repository.as_ref().is_some_and(|current| {
                            current.ssh.is_some()
                                && current.root == repo.root
                                && current.storage_key() == repo.storage_key()
                        }) {
                            this.reconnect_repository(repo, window, cx);
                            if let Some(path) = file {
                                if this.view == WorkspaceView::Docs {
                                    this.open_doc(path, None, window, cx);
                                } else if this.view == WorkspaceView::Editor {
                                    this.open_file(path, false, window, cx);
                                }
                            }
                        } else {
                            this.apply_repository(repo, session, file, docs, window, cx);
                            if reused && this.view != WorkspaceView::Docs {
                                this.rescan_workspace(window, cx);
                            }
                        }
                        this.flush_session(cx)?;
                        if let Some(repo) = &this.repository {
                            this.store.remember_repository(repo)?;
                            this.repository_ui.record_open(repo);
                        }
                    } else {
                        if let Some(repo) = &this.repository
                            && !this.repository_ui.has_recent(repo)
                        {
                            this.store.remember_repository(repo)?;
                            this.repository_ui.record_open(repo);
                        }
                        this.dismiss_repository_picker(window, cx);
                        this.repository_ui.menu_open = false;
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    let message = Some(format!("{error:#}"));
                    if from_menu {
                        this.repository_ui.menu_error = message;
                    } else {
                        this.repository_ui.error = message;
                    }
                    if this.repository.is_none() {
                        this.show_repository_start(window, cx);
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn watch_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.connection_watch = Task::ready(());
        if self.view == WorkspaceView::Docs {
            self.repository_watch = Task::ready(());
            self.repository_watch_active = false;
            self.repository_watch_pending = true;
        } else {
            self.repository_watch_pending = false;
            self.watch_repository(window, cx);
        }
        let ssh = self.repository.as_ref().and_then(|repo| repo.ssh.clone());
        if ssh.is_none() && self.recent_ssh_repositories.is_empty() {
            return;
        }
        self.connection_watch = cx.spawn_in(window, async move |this, cx| {
            let mut ssh = ssh;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let error = ssh.as_ref().and_then(|ssh| ssh.failure());
                if error.is_some() {
                    ssh = None;
                }
                let keep_watching = this.update_in(cx, |this, window, cx| {
                    let count = this.recent_ssh_repositories.len();
                    this.recent_ssh_repositories.retain(|repo| {
                        repo.ssh.as_ref().is_some_and(|ssh| ssh.failure().is_none())
                    });
                    if count != this.recent_ssh_repositories.len() {
                        cx.notify();
                    }
                    if let Some(error) = error {
                        this.language_servers.retain(|key, _| !key.is_remote());
                        let paths = this
                            .files
                            .keys()
                            .filter(|path| !path.is_absolute())
                            .cloned()
                            .collect::<Vec<_>>();
                        for path in paths {
                            this.connect_file_to_language_service(&path, cx);
                        }
                        toast::error(
                            window,
                            cx,
                            t!("workspace.ssh_disconnected_detail", error = error),
                        );
                        cx.notify();
                    }
                    ssh.is_some() || !this.recent_ssh_repositories.is_empty()
                });
                if !keep_watching.unwrap_or(false) {
                    break;
                }
            }
        });
    }

    fn reconnect_repository(
        &mut self,
        repo: Repository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_locator(window, cx);
        self.tree_scroll = UniformListScrollHandle::new();
        self.cancel_file_open();
        self.audio_preview.stop();
        self.dismiss_repository_picker(window, cx);
        self.repository_ui.menu_open = false;
        self.repository_ui.reset_worktrees();
        self.doc_task = Task::ready(());
        self.doc_loading = None;
        self.doc_error = None;
        self.open_documents_list = false;
        self.reset_workspace_tree();
        self.repository = Some(Arc::new(repo));
        self.git = None;
        self.git_edit.clear();
        self.git_history.busy = false;
        self.git_watch = Task::ready(());
        self.language_servers.retain(|key, _| !key.is_remote());
        self.rebuild_tree();
        self.watch_connection(window, cx);
        self.load_docs_tree(window, cx);
        self.reveal_file_in_tree(window, cx);
        let repo = self.repository.clone().unwrap();
        let paths = self
            .files
            .keys()
            .filter(|path| !path.is_absolute())
            .cloned()
            .collect::<Vec<_>>();
        let images: Vec<_> = self
            .images
            .keys()
            .filter(|path| !path.is_absolute())
            .cloned()
            .collect();
        for path in images {
            self.load_image_tab(path, window, cx);
        }
        let check_repo = repo.clone();
        let check = cx.background_spawn(async move {
            paths
                .into_iter()
                .map(|path| {
                    let disk = Document::read(&check_repo, &path);
                    let project = ServerProject::for_file(&check_repo, &path);
                    (path, disk, project)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let updates = check.await;
            _ = this.update_in(cx, move |this, window, cx| {
                if !this
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                {
                    return;
                }
                let mut keys = std::collections::HashSet::new();
                for (path, disk, project) in updates {
                    let Some(file) = this.files.get_mut(&path) else {
                        continue;
                    };
                    if !this.saving.contains(&path)
                        && let Ok(disk) = disk
                    {
                        let modified = file.committed != file.saved;
                        let confirmed = file
                            .unconfirmed_save
                            .take()
                            .is_some_and(|text| disk.saved_text == text.to_string());
                        if disk.draft_baseline() != file.document.draft_baseline() {
                            if !modified
                                || disk.saved_text == file.committed.to_string()
                                || confirmed
                            {
                                let update_editor =
                                    !modified && file.committed.to_string() != disk.saved_text;
                                file.saved = Rope::from(disk.saved_text.as_str());
                                file.document = disk;
                                if update_editor {
                                    file.committed = file.saved.clone();
                                    let text = file.document.saved_text.clone();
                                    file.editor.update(cx, |editor, cx| {
                                        editor.set_value(text, window, cx)
                                    });
                                }
                                file.dirty = file.committed != file.saved;
                                file.pending_draft = true;
                                file.revision += 1;
                                if let Err(error) = file.flush_draft(&repo, &this.store) {
                                    toast::error(window, cx, format!("{error:#}"));
                                }
                            } else {
                                if let Err(error) = file.flush_draft(&repo, &this.store) {
                                    toast::error(window, cx, format!("{error:#}"));
                                }
                                file.document.conflict = true;
                            }
                        }
                    }
                    if let Some(project) = &project {
                        keys.insert(project.key.clone());
                    }
                    file.server_project = project;
                }
                for key in keys {
                    this.start_language_service(&key, cx);
                }
                cx.notify();
            });
        })
        .detach();
        if self.view == WorkspaceView::Git {
            self.open_git_panel(window, cx);
        } else if self.view == WorkspaceView::Docs {
            if let Some(path) = self
                .doc
                .as_ref()
                .map(|doc| doc.read(cx).path.clone())
                .or_else(|| self.session.document.clone())
                .or_else(|| self.initial_document())
            {
                self.open_doc(path, None, window, cx);
            }
        } else {
            self.restore_active_file(window, cx);
        }
    }

    fn apply_repository(
        &mut self,
        repo: Repository,
        mut session: WorktreeSession,
        file: Option<PathBuf>,
        prepared_docs: Option<DocumentTree>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cache_docs(cx);
        let cached_docs = self
            .recent_docs
            .iter()
            .position(|cached| cached.key == repo.storage_key())
            .and_then(|index| self.recent_docs.remove(index));
        self.tree_status_task = Task::ready(());
        self.recent_ssh_repositories
            .retain(|cached| cached.root != repo.root);
        let previous = self
            .repository_snapshot
            .take()
            .or_else(|| self.repository.clone());
        if let Some(previous) = previous.filter(|previous| {
            previous.root != repo.root
                && previous
                    .ssh
                    .as_ref()
                    .is_some_and(|ssh| ssh.failure().is_none())
        }) {
            self.recent_ssh_repositories.push_front(previous);
            self.recent_ssh_repositories
                .truncate(RECENT_REPOSITORY_LIMIT);
        }
        self.dismiss_locator(window, cx);
        self.tree_scroll = UniformListScrollHandle::new();
        self.cancel_file_open();
        self.dismiss_repository_picker(window, cx);
        self.repository_ui.menu_open = false;
        self.repository_ui.reset_worktrees();
        if let Some(view) = self.startup_view.take() {
            self.view = view;
        } else if file.is_some() {
            self.view = WorkspaceView::Editor;
        } else if self
            .repository
            .as_ref()
            .is_none_or(|current| current.common_dir != repo.common_dir)
        {
            self.view = session.view.unwrap_or_default();
        }
        if self
            .repository
            .as_ref()
            .is_some_and(|current| current.storage_key() == repo.storage_key())
        {
            // The target session was read before flushing the current UI state.
            session.commit_draft = self.session.commit_draft.take();
            session.panels = self.session.panels;
            session.source_history = std::mem::take(&mut self.session.source_history);
        }
        session.source_history.normalize();
        self.source_caret = None;
        self.source_click = None;
        self.tabs = session.tabs.clone();
        self.session = session;
        self.restore_panel_preferences();
        self.session_revision += 1;
        self.files.clear();
        self.language_action_groups.clear();
        self.git_history = Default::default();
        self.language_rename = Task::ready(());
        self.language_format = Task::ready(());
        self.image_preview = None;
        self.image_preview_return_focus = None;
        for (_, mut image) in self.images.drain() {
            image.release(window);
        }
        self.audio_preview.stop();
        self.review_loads.clear();
        self.language_servers.clear();
        self.lsp_panel_open = false;
        self.tab_scroll = ScrollHandle::new();
        self.tab_viewport_width = px(0.);
        self.tabs_overflow = false;
        self.open_files_list = false;
        self.open_files_scroll = UniformListScrollHandle::new();
        self.close_queue.clear();
        self.expanded.clear();
        self.git = None;
        self.git_edit.clear();
        self.git_watch = Task::ready(());
        self.doc = None;
        self.doc_task = Task::ready(());
        self.doc_loading = None;
        self.doc_error = None;
        self.open_documents_list = false;
        self.open_documents_scroll = UniformListScrollHandle::new();
        self.docs_expanded.clear();
        self.focus.focus(window, cx);
        let title = if let Some(ssh) = &repo.ssh {
            format!("{} · {} · Stroll", repo.name, ssh.target().authority())
        } else {
            format!("{} · Stroll", repo.name)
        };
        window.set_window_title(&title);
        self.reset_workspace_tree();
        if let Some(cached) = cached_docs {
            self.docs_tree = cached.tree;
            self.docs_tree_seeded = true;
            self.docs_tree_complete = cached.complete;
            self.docs_expanded = cached.expanded;
            if self.view == WorkspaceView::Docs {
                self.doc = cached.reader.filter(|reader| {
                    self.session.document.as_deref() == Some(&reader.read(cx).path)
                });
                if let Some(reader) = &self.doc {
                    reader.update(cx, |reader, cx| {
                        reader.set_typography(self.config.current().docs, cx);
                        reader.set_outline_layout(
                            self.outline_width,
                            self.outline_visibility,
                            false,
                        );
                    });
                }
                self.tree_scroll = cached.scroll;
            }
        } else if let Some(tree) = prepared_docs {
            self.docs_tree = Rc::new(tree);
            self.docs_tree_seeded = true;
        }
        self.repository = Some(Arc::new(repo));
        self.watch_connection(window, cx);
        self.rebuild_tree();
        match self.view {
            WorkspaceView::Docs => {
                if let Some(path) = file
                    .or_else(|| self.session.document.clone())
                    .or_else(|| self.initial_document())
                {
                    self.open_doc(path, None, window, cx);
                }
                self.load_docs_tree(window, cx);
            }
            WorkspaceView::Git => self.open_git_panel(window, cx),
            WorkspaceView::Editor => {
                if let Some(file) = file {
                    self.request_file(file, FileOpen::Startup, window, cx);
                } else {
                    self.restore_active_file(window, cx);
                }
            }
        }
    }

    fn rebuild_tree(&mut self) {
        fn visit<'a>(
            children_of: &impl Fn(&Path) -> Option<&'a [Entry]>,
            path: &Path,
            depth: usize,
            ignored_ancestor: bool,
            continuing_guides: u16,
            expanded: &BTreeSet<PathBuf>,
            rows: &mut Vec<TreeRow>,
        ) {
            if let Some(children) = children_of(path) {
                for (index, entry) in children.iter().enumerate() {
                    let guides = file_tree::TreeGuides::new(
                        continuing_guides,
                        depth,
                        index + 1 == children.len(),
                    );
                    let open = entry.directory && expanded.contains(&entry.path);
                    rows.push(TreeRow {
                        entry: entry.clone(),
                        depth,
                        expanded: open,
                        dimmed: entry.ignored && !ignored_ancestor,
                        guides,
                    });
                    if open {
                        visit(
                            children_of,
                            &entry.path,
                            depth + 1,
                            ignored_ancestor || entry.ignored,
                            guides.continuing(),
                            expanded,
                            rows,
                        );
                    }
                }
            }
        }
        let mut rows = Vec::new();
        if self.view == WorkspaceView::Docs {
            visit(
                &|path| self.docs_tree.children.get(path).map(Vec::as_slice),
                Path::new(""),
                0,
                false,
                0,
                &self.docs_expanded,
                &mut rows,
            );
        } else if let Some(repo) = self
            .repository_snapshot
            .as_ref()
            .or(self.repository.as_ref())
        {
            visit(
                &|path| repo.children.get(path).map(Vec::as_slice),
                Path::new(""),
                0,
                false,
                0,
                &self.expanded,
                &mut rows,
            );
        }
        self.rows = Rc::new(rows);
    }

    fn tree_click(
        &mut self,
        entry: Entry,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view == WorkspaceView::Editor
            && (entry.directory || entry.symlink || !audio_preview::is_audio(&entry.path))
        {
            if event.click_count() < 2 {
                self.focus_tree(entry.path.clone(), window, cx);
            } else if self.tree_focus.is_focused(window) {
                self.focus.focus(window, cx);
            }
        }
        if entry.directory {
            self.tree_motion.get_mut().start();
            let expanded = if self.view == WorkspaceView::Docs {
                &mut self.docs_expanded
            } else {
                &mut self.expanded
            };
            if !expanded.remove(&entry.path) {
                expanded.insert(entry.path.clone());
                if self.view == WorkspaceView::Editor {
                    self.load_tree_directory(entry.path, window, cx);
                }
            } else if self.view == WorkspaceView::Editor
                && self
                    .tree_reveal
                    .as_ref()
                    .is_some_and(|path| path.starts_with(&entry.path))
            {
                self.tree_reveal = None;
            }
            self.rebuild_tree();
            if self
                .tree_edit
                .as_ref()
                .is_some_and(|edit| edit.row(&self.rows).is_none())
                && !self.tree_busy
            {
                self.finish_tree_edit(window, cx);
            }
            cx.notify();
        } else if entry.symlink {
        } else if self.view == WorkspaceView::Editor && audio_preview::is_audio(&entry.path) {
            if event.click_count() < 2 {
                self.preview_audio(entry.path, window, cx);
            }
        } else if self.view == WorkspaceView::Docs {
            self.open_doc(entry.path, None, window, cx);
        } else {
            self.open_file(entry.path, preview_click(event), window, cx);
        }
    }

    fn switch_view(&mut self, view: WorkspaceView, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_view_to(view, None, window, cx);
    }

    fn switch_view_to(
        &mut self,
        view: WorkspaceView,
        document: Option<(PathBuf, Option<Task<Result<docs::PreparedDocument>>>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view == view || self.confirming_close {
            return;
        }
        if self.repository_ui.picker_open || self.repository_ui.opening.is_some() {
            return;
        }
        self.close_locator(false, window, cx);
        self.repository_ui.menu_open = false;
        self.language_rename = Task::ready(());
        self.language_format = Task::ready(());
        if let Some(file) = self.active_file() {
            file.editor
                .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
        }
        self.queue_drafts(window, cx);
        self.capture_session(cx);
        self.cancel_file_open();
        self.tree_scroll = UniformListScrollHandle::new();
        if view != WorkspaceView::Docs
            && let Some(doc) = &self.doc
        {
            doc.update(cx, |doc, cx| doc.release_online_images(cx));
        }
        self.finish_tree_edit(window, cx);
        self.view = view;
        self.git_watch = Task::ready(());
        self.lsp_panel_open = false;
        self.open_files_list = false;
        self.open_documents_list = false;
        self.rebuild_tree();
        if view == WorkspaceView::Docs {
            self.load_docs_tree(window, cx);
            if let Some((path, prepared)) = document {
                self.open_doc_at(path, None, None, prepared, window, cx);
            } else if self.doc.is_none() && self.doc_loading.is_none() && self.doc_error.is_none() {
                let path = self
                    .session
                    .document
                    .clone()
                    .or_else(|| {
                        (self.session.open_documents.is_none())
                            .then(|| {
                                self.tabs
                                    .active()
                                    .filter(|path| docs::is_markdown(path))
                                    .map(Path::to_owned)
                            })
                            .flatten()
                    })
                    .or_else(|| self.initial_document());
                if let Some(path) = path {
                    self.open_doc(path, None, window, cx);
                }
            }
            self.focus.focus(window, cx);
        } else if view == WorkspaceView::Git {
            self.open_git_panel(window, cx);
        } else {
            self.restore_active_file(window, cx);
        }
        cx.notify();
    }

    fn open_git_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = &self.repository else {
            self.focus.focus(window, cx);
            return;
        };
        if let Some(panel) = &self.git {
            if !self.repository_watch_active {
                panel.update(cx, |panel, cx| panel.refresh_worktree(window, cx));
            }
        } else {
            let root = repo.root.clone();
            let backend = repo.git_backend();
            let workspace = cx.weak_entity();
            let config = self.config.current().git;
            let inlay_hints = self.config.current().editor.inlay_hints;
            self.git = Some(cx.new(|cx| {
                let mut panel =
                    GitPanel::new(root, backend, workspace, config, &self.session, window, cx);
                panel.set_inlay_hints_enabled(inlay_hints, window, cx);
                panel
            }));
        }
        if let Some(panel) = &self.git {
            panel.update(cx, |panel, cx| {
                panel.focus(window, cx);
                panel.schedule_snapshot_alignment(cx);
            });
        }
    }

    fn open_doc(
        &mut self,
        path: PathBuf,
        fragment: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_doc_at(path, fragment, None, None, window, cx);
    }

    /// Opens a Markdown file in the document view without requiring it to be
    /// listed in the repository's document tree.
    fn open_markdown_in_docs(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !docs::is_markdown(&path) {
            return;
        }
        if self.view == WorkspaceView::Docs {
            self.open_doc(path, None, window, cx);
        } else {
            self.switch_view_to(WorkspaceView::Docs, Some((path, None)), window, cx);
        }
    }

    fn open_doc_at(
        &mut self,
        path: PathBuf,
        fragment: Option<String>,
        position: Option<(std::ops::Range<usize>, [u8; 32])>,
        prepared: Option<Task<Result<docs::PreparedDocument>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.repository_ui.opening.is_some()
            || (self.repository.is_none() && !(self.standalone.is_some() && path.is_absolute()))
        {
            return;
        }
        if prepared.is_none() {
            if let Some(pending) = &mut self.doc_loading
                && pending.path == path
            {
                if fragment.is_some() || position.is_some() {
                    pending.fragment = fragment;
                    pending.position = position;
                }
                return;
            }
            if let Some(doc) = &self.doc
                && doc.read(cx).path == path
            {
                let previous_error = self.doc_error.take();
                if let Some(fragment) = &fragment {
                    if !doc.update(cx, |doc, cx| doc.jump_to_fragment(fragment, cx)) {
                        self.doc_error =
                            Some(t!("docs.fragment_missing", fragment = fragment).into_owned());
                    }
                }
                if let Some((range, fingerprint)) = position {
                    doc.update(cx, |doc, cx| doc.locate_source(range, fingerprint, cx));
                }
                if self.doc_error != previous_error {
                    cx.notify();
                }
                return;
            }
        }
        let repo = self.repository.clone();
        let standalone = self.standalone.is_some();
        let local_text = self.local_document_text(&path);
        let store = self.store.clone();
        self.capture_document_position(cx);
        let saved_position = self.session.documents.get(&path).copied();
        self.session.select_document(&path);
        self.session_revision += 1;
        self.doc = None;
        self.doc_error = None;
        let root = repo.as_ref().map_or_else(
            || path.parent().unwrap_or(Path::new("")).to_owned(),
            |repo| repo.root.clone(),
        );
        let load = prepared.unwrap_or_else(|| {
            let path = path.clone();
            cx.background_spawn(async move {
                Self::prepare_document(repo.as_deref(), &path, &store, local_text)
            })
        });
        self.doc_loading = Some(DocumentOpen {
            path,
            fragment,
            position,
        });
        self.doc_task = cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            _ = this.update_in(cx, move |this, window, cx| {
                let Some(DocumentOpen {
                    path,
                    fragment,
                    position,
                }) = this.doc_loading.take()
                else {
                    return;
                };
                match result {
                    Ok(document) => {
                        if let Some(entry) = this
                            .session
                            .open_documents
                            .as_mut()
                            .and_then(|docs| docs.iter_mut().find(|entry| entry.path == path))
                            && entry.title.as_deref() != document.title()
                        {
                            entry.title = document.title().map(str::to_owned);
                            this.session_revision += 1;
                        }
                        if !path.is_absolute() {
                            this.docs_expanded.extend(
                                path.ancestors()
                                    .skip(1)
                                    .filter(|parent| !parent.as_os_str().is_empty())
                                    .map(Path::to_owned),
                            );
                        }
                        this.locator_visit(&path);
                        let workspace = cx.entity().downgrade();
                        let typography = this.config.current().docs;
                        let restore_position = fragment.is_none() && position.is_none();
                        let reader = cx.new(|cx| {
                            let mut reader = DocReader::new(
                                root, path, document, fragment, workspace, window, cx,
                            );
                            reader.set_typography(typography, cx);
                            reader.set_outline_layout(
                                this.outline_width,
                                this.outline_visibility,
                                standalone,
                            );
                            if restore_position {
                                reader.restore_position(saved_position);
                            }
                            if let Some((range, fingerprint)) = position {
                                reader.locate_source(range, fingerprint, cx);
                            }
                            reader
                        });
                        cx.observe(&reader, |_, _, cx| cx.notify()).detach();
                        this.doc = Some(reader);
                        this.rebuild_tree();
                    }
                    Err(error) => {
                        this.doc_error = Some(format!("{error:#}"));
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    pub(crate) fn follow_document_link(
        &mut self,
        from: &Path,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let destination =
            docs::destination(from, target).and_then(|destination| match destination {
                Destination::Local { path, fragment } if path.is_absolute() => {
                    Some(Destination::Local {
                        path: if let Some(repo) = &self.repository {
                            repo.local_file_key(&path).ok()?
                        } else {
                            path.canonicalize().ok()?
                        },
                        fragment,
                    })
                }
                destination => Some(destination),
            });
        match destination {
            Some(Destination::External(url)) => cx.open_url(url.as_str()),
            Some(Destination::Local { path, fragment })
                if self
                    .standalone
                    .as_ref()
                    .is_some_and(|current| *current != path) =>
            {
                self.open_local_window(path, fragment, window, cx);
            }
            Some(Destination::Local { path, fragment }) if docs::is_markdown(&path) => {
                self.open_doc(path, fragment, window, cx);
            }
            Some(Destination::Local { path, fragment }) => {
                self.open_document_source(path.clone(), window, cx);
                let line = fragment.as_deref().and_then(docs::line_fragment);
                if self.view == WorkspaceView::Editor
                    && let Some(file) = self.active_file().filter(|file| file.path == path)
                    && let Some(line) = line
                {
                    file.editor.update(cx, |editor, cx| {
                        editor.set_cursor_position(
                            gpui_component::input::Position::new(line, 0),
                            window,
                            cx,
                        );
                    });
                }
            }
            None => {
                self.doc_error = Some(t!("workspace.link_failed").into());
                cx.notify();
            }
        }
    }

    pub(crate) fn open_document_source(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_document_source_at(path, None, window, cx);
    }

    fn open_document_source_at(
        &mut self,
        path: PathBuf,
        position: Option<SourcePosition>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.switch_view(WorkspaceView::Editor, window, cx);
        if self.view == WorkspaceView::Editor {
            if position.is_some() {
                self.request_file(
                    path,
                    FileOpen::Select {
                        preview: true,
                        position,
                    },
                    window,
                    cx,
                );
            } else {
                self.open_file(path, true, window, cx);
            }
            cx.notify();
        }
    }

    fn open_file(
        &mut self,
        path: PathBuf,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.repository_ui.opening.is_none() && !self.confirming_close {
            self.request_file(
                path,
                FileOpen::Select {
                    preview,
                    position: None,
                },
                window,
                cx,
            );
        }
    }

    fn cancel_file_open(&mut self) {
        self.file_task = Task::ready(());
        self.loading_file = None;
    }

    fn request_file(
        &mut self,
        path: PathBuf,
        request: FileOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(request, FileOpen::Startup | FileOpen::Restore) && self.terminal_maximized() {
            self.restore_terminal_split();
            cx.notify();
        }
        if let Some((loading, pending)) = &mut self.loading_file
            && loading == &path
        {
            match request {
                FileOpen::Select { .. } | FileOpen::Locate(_)
                    if matches!(pending, FileOpen::History { .. }) =>
                {
                    *pending = request;
                }
                FileOpen::Select {
                    preview: true,
                    position: None,
                }
                | FileOpen::Startup
                | FileOpen::Restore => {}
                FileOpen::Select {
                    preview: false,
                    position: None,
                } => match pending {
                    FileOpen::Select { preview, .. } => *preview = false,
                    FileOpen::Locate(_) => {}
                    FileOpen::History { .. } | FileOpen::Startup | FileOpen::Restore => {
                        *pending = request
                    }
                },
                _ => *pending = request,
            }
            return;
        }
        if self.loading_file.is_none()
            && self.tabs.active() == Some(&path)
            && (self.files.contains_key(&path) || self.images.contains_key(&path))
            && let FileOpen::Select {
                preview,
                position: None,
            } = request
        {
            if !preview {
                self.keep_tab(&path, cx);
            }
            return;
        }
        self.cancel_file_open();
        if self.files.contains_key(&path) {
            self.select_loaded_file(path, request, window, cx);
            return;
        }
        if self.repository.is_some() && crate::image_viewer::is_image(&path) {
            if !self.images.contains_key(&path) {
                self.load_image_tab(path.clone(), window, cx);
            }
            self.select_loaded_file(path, request, window, cx);
            return;
        }
        let load = if let Some(repo) = self.repository.clone() {
            self.load_file_buffer(repo, path.clone(), cx)
        } else if self.standalone.is_some() && path.is_absolute() {
            let path = path.clone();
            let store = self.store.clone();
            cx.background_spawn(async move {
                let buffer = FileBuffer::new(Document::open_local(&path, &store)?);
                Ok((buffer, ServerProject::for_local_file(&path)))
            })
        } else {
            return;
        };
        self.loading_file = Some((path, request));
        self.file_task = cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            _ = this.update_in(cx, |this, window, cx| {
                let Some((path, request)) = this.loading_file.take() else {
                    return;
                };
                match result {
                    Ok((buffer, project)) => {
                        if !this.files.contains_key(&path) {
                            this.install_file(&path, buffer, project, window, cx);
                        }
                        this.select_loaded_file(path, request, window, cx);
                    }
                    Err(error) => {
                        if let FileOpen::History { index, .. } = &request
                            && error.chain().any(|cause| {
                                cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                                    error.kind() == std::io::ErrorKind::NotFound
                                })
                            })
                        {
                            this.skip_missing_source(&path, *index, window, cx);
                            cx.notify();
                            return;
                        }
                        match request {
                            FileOpen::Restore => {
                                if this
                                    .repository
                                    .as_ref()
                                    .and_then(|repo| repo.ssh.as_ref())
                                    .is_none_or(|ssh| ssh.failure().is_none())
                                {
                                    this.tabs.close(&path);
                                    this.restore_active_file(window, cx);
                                }
                            }
                            FileOpen::Startup => this.restore_active_file(window, cx),
                            FileOpen::Select { .. }
                            | FileOpen::Locate(_)
                            | FileOpen::History { .. } => {}
                        }
                        this.show_error(error, window, cx);
                        cx.notify();
                    }
                }
            });
        });
        cx.notify();
    }

    fn select_loaded_file(
        &mut self,
        path: PathBuf,
        request: FileOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let navigate = self.view == WorkspaceView::Editor
            && !matches!(request, FileOpen::Startup | FileOpen::Restore);
        let source = navigate.then(|| self.source_location(cx)).flatten();
        let same_file = self.tabs.active() == Some(path.as_path());
        let (preview, position, location, replay) = match request {
            FileOpen::Select { preview, position } => (preview, position, None, None),
            FileOpen::Locate(location) => (false, None, location, None),
            FileOpen::History { index, position } => (true, None, None, Some((index, position))),
            FileOpen::Startup | FileOpen::Restore => (true, None, None, None),
        };
        self.select_file_tab(&path, preview, window, cx);
        if let Some(position) = position
            && let Some(file) = self.files.get(&path)
        {
            use gpui_component::input::RopeExt as _;
            let text = file.editor.read(cx).text();
            let offset = match position {
                SourcePosition::Lsp(position) => lsp::editor_position(text, position)
                    .map(|position| text.position_to_offset(&position)),
                SourcePosition::MarkdownFragment(fragment) => {
                    let offset = docs::heading_source_offset(&text.to_string(), &fragment);
                    if offset.is_none() {
                        toast::info(window, cx, t!("docs.fragment_missing", fragment = fragment));
                    }
                    offset
                }
                SourcePosition::Diff(source) => {
                    self.reveal_diff_source(
                        path.clone(),
                        source,
                        navigate && same_file,
                        window,
                        cx,
                    );
                    None
                }
            };
            if let Some(offset) = offset {
                let file = self.files.get_mut(&path).unwrap();
                file.restore_position = None;
                file.editor.update(cx, |editor, cx| {
                    reveal_editor_range(editor, offset..offset, None, window, cx);
                });
            }
        }
        if let Some((range, fingerprint)) = location
            && let Some(file) = self.files.get_mut(&path)
        {
            file.restore_position = None;
            let applied = file.editor.update(cx, |editor, cx| {
                let text = editor.text();
                if crate::locator::fingerprint(&text.to_string()) != fingerprint
                    || range.end > text.len()
                {
                    return false;
                }
                reveal_editor_range(editor, range, None, window, cx);
                true
            });
            if !applied {
                toast::info(window, cx, t!("locator.stale"));
            }
        }
        if let Some((index, position)) = replay {
            self.complete_source_replay(&path, index, position, source, cx);
        } else if navigate {
            self.record_source_navigation(source, cx);
        }
        self.locator_visit(&path);
    }

    fn reveal_diff_source(
        &mut self,
        path: PathBuf,
        source: DiffSource,
        record_delayed_jump: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let file = self.files.get_mut(&path).unwrap();
        let DiffSource { row, snapshot } = source;
        let Some(snapshot) = snapshot else {
            Self::reveal_diff_row(file, row, window, cx);
            return;
        };
        let editor = file.editor.clone();
        let current = editor.read(cx);
        let revision = current.document_revision();
        let selection = current.selected_range();
        let current = current.text().clone();
        let mapping = cx.background_spawn(async move {
            let Some(snapshot) = snapshot.await else {
                return row;
            };
            line_alignment::LineAlignment::between(&snapshot, &current)
                .and_then(|alignment| alignment.approximate_current_row(row))
                .unwrap_or(row)
        });
        self.file_task = cx.spawn_in(window, async move |this, cx| {
            let mapped = mapping.await;
            _ = this.update_in(cx, |this, window, cx| {
                if this.view != WorkspaceView::Editor || this.tabs.active() != Some(path.as_path())
                {
                    return;
                }
                let source = record_delayed_jump
                    .then(|| this.source_location(cx))
                    .flatten();
                let Some(file) = this
                    .files
                    .get_mut(&path)
                    .filter(|file| file.editor == editor)
                else {
                    return;
                };
                let state = editor.read(cx);
                if state.selected_range() != selection || !state.focus_handle(cx).is_focused(window)
                {
                    return;
                }
                let target = if state.document_revision() == revision {
                    mapped
                } else {
                    row
                };
                Self::reveal_diff_row(file, target, window, cx);
                if record_delayed_jump {
                    this.record_source_navigation(source, cx);
                }
            });
        });
    }

    fn reveal_diff_row(
        file: &mut OpenFile,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::input::RopeExt as _;

        file.restore_position = None;
        file.editor.update(cx, |editor, cx| {
            let text = editor.text();
            let offset = text.line_start_offset(row.min(text.lines_len().saturating_sub(1)));
            reveal_editor_range(editor, offset..offset, None, window, cx);
        });
    }

    fn select_file_tab(
        &mut self,
        path: &Path,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capture_active_file_position(cx);
        let previous = self.tabs.active().map(Path::to_owned);
        let preview = preview && !path.is_absolute() && !self.tab_is_dirty(path);
        if let Some(replaced) = self.tabs.open(path.to_owned(), preview) {
            self.files.remove(&replaced);
            self.remove_image_tab(&replaced, window);
        }
        self.finish_tab_selection(previous.as_deref(), window, cx);
    }

    fn load_file_buffer(
        &self,
        repo: Arc<Repository>,
        path: PathBuf,
        cx: &App,
    ) -> Task<Result<(FileBuffer, Option<ServerProject>)>> {
        let store = self.store.clone();
        cx.background_spawn(async move {
            let buffer = FileBuffer::new(Document::open(&repo, &path, &store)?);
            let project = ServerProject::for_file(&repo, &path);
            Ok((buffer, project))
        })
    }

    fn ensure_review_buffer(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        complete: impl FnOnce(&mut Self, Result<()>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if self.files.contains_key(&path) {
            complete(self, Ok(()), window, cx);
            return;
        }
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let pending = self.review_loads.entry(path.clone()).or_default();
        pending.push(Box::new(complete));
        if pending.len() > 1 {
            return;
        }
        let load = self.load_file_buffer(repo.clone(), path.clone(), cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            _ = this.update_in(cx, |this, window, cx| {
                if !this
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                {
                    return;
                }
                let completions = this.review_loads.remove(&path).unwrap_or_default();
                let result = result.map(|(buffer, project)| {
                    if !this.files.contains_key(&path) {
                        this.install_file(&path, buffer, project, window, cx);
                    }
                });
                let error = result.err().map(|error| format!("{error:#}"));
                for complete in completions {
                    complete(
                        this,
                        error
                            .as_ref()
                            .map_or(Ok(()), |error| Err(anyhow::anyhow!("{error}"))),
                        window,
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn document_repository(&self) -> Option<Arc<Repository>> {
        self.repository.clone()
    }

    fn source_resources(&self, path: &Path) -> &crate::language_resources::LanguageResources {
        if let Some(repo) = self.repository.as_ref().filter(|_| !path.is_absolute()) {
            repo.language_resources()
        } else {
            crate::language_resources::LanguageResources::installed()
        }
    }

    fn source_language(&self, path: &Path, source: &Rope) -> SharedString {
        self.source_resources(path)
            .for_path(path)
            .unwrap_or_else(|| language_for_source(path, source).into())
    }

    fn install_file(
        &mut self,
        path: &Path,
        buffer: FileBuffer,
        project: Option<ServerProject>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let language = self.source_language(path, &buffer.text);
        self.source_resources(path).configure_editor(path, cx);
        let wrap = self.wrap;
        let line_numbers = self.config.current().editor.line_numbers;
        let key = project.as_ref().map(|project| project.key.clone());
        let FileBuffer {
            document,
            text,
            saved,
        } = buffer;
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .progressive_wrap()
                .line_clipboard()
                .line_commands()
                .navigable_overview_ruler()
                .language(language)
                .soft_wrap(wrap)
                .line_number(line_numbers)
                .default_file_text(text)
        });
        let restore_position = self.session.files.get(path).copied();
        let history = self.history_handler(path, cx);
        editor.update(cx, |editor, cx| {
            if docs::is_markdown(path)
                && let Some(handler) = markdown_editing::newline_edit_handler()
            {
                editor.set_newline_edit_handler(handler);
            }
            editor.set_grouped_history_handler(history);
            if let Err(error) = self.restore_editor_history(path, &document, editor) {
                eprintln!("Could not load editor history: {error:#}");
            }
            if let Some(position) = restore_position {
                session::restore_file_selection(editor, position, cx);
            }
            editor.ensure_highlighter_factory(
                gpui_component::highlighter::input_highlighter_factory(),
            );
            editor.prepare_language(window, cx);
        });
        let caret_hints = editor.update(cx, |editor, cx| caret_hints::CaretHints::new(editor, cx));
        let git = editor_git::EditorGit::new(&editor, cx);
        let inlay_hints = inlay_hints::InlayHints::new(&editor, cx);
        inlay_hints.set_enabled(self.config.current().editor.inlay_hints, cx);
        let changes = cx.subscribe_in(
            &editor,
            window,
            move |this, editor, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.editor_changed(editor.clone(), window, cx),
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false,
                } => {
                    this.indent_source_newline(&editor, window, cx);
                }
                InputEvent::GutterClick(action) => {
                    if let Some(path) = this
                        .files
                        .values()
                        .find(|file| &file.editor == editor)
                        .map(|file| file.path.clone())
                    {
                        this.toggle_editor_git(&path, *action, cx);
                    }
                }
                _ => {}
            },
        );
        let editor_observer = self.observe_source_editor(&editor, window, cx);
        let input_edits = self.track_editor_edits(&editor, window, cx);
        let committed = editor.read(cx).text().clone();
        let dirty = document.restored;
        self.files.insert(
            path.to_owned(),
            OpenFile {
                path: path.to_owned(),
                document,
                editor,
                committed,
                saved,
                draft_write: Arc::default(),
                revision: 1,
                queued_revision: None,
                _changes: changes,
                _editor_observer: editor_observer,
                _input_edits: input_edits,
                newline: None,
                newline_task: Task::ready(()),
                server_project: project,
                language_server: None,
                _lsp_refresh: Task::ready(()),
                inlay_hints,
                caret_hints,
                dirty,
                pending_draft: false,
                unconfirmed_save: None,
                missing: false,
                sync_revision: 0,
                wrap,
                git,
                restore_position,
            },
        );
        if self.startup_lsp.enabled
            && let Some(key) = key
            && !self.language_servers.contains_key(&key)
        {
            self.start_language_service(&key, cx);
        } else {
            self.connect_file_to_language_service(path, cx);
        }
    }

    fn restore_active_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Only the selected buffer is needed at startup, even for large tab sets.
        if let Some(path) = self.tabs.active().map(Path::to_owned) {
            self.request_file(path, FileOpen::Restore, window, cx);
        } else {
            self.finish_tab_selection(None, window, cx);
        }
    }

    fn language_source_path(
        repo: Option<&Repository>,
        uri: &str,
        local_source: bool,
    ) -> Option<PathBuf> {
        if !local_source {
            return repo?.source_path(uri);
        }
        let path = url::Url::parse(uri).ok()?.to_file_path().ok()?;
        if let Some(repo) = repo {
            repo.local_file_key(&path).ok()
        } else {
            crate::repository::canonical_local_file(&path).ok()
        }
    }

    fn open_definition(
        &mut self,
        params: lsp_types::ShowDocumentParams,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.view, WorkspaceView::Editor | WorkspaceView::Git)
            || self.repository_ui.opening.is_some()
            || self.repository_ui.picker_open
        {
            return;
        }
        let uri = params.uri.as_str();
        let markdown_source = self.view == WorkspaceView::Editor
            && self
                .active_file()
                .is_some_and(|file| docs::is_markdown(&file.path));
        let fragment = markdown_source
            .then(|| uri.split_once('#'))
            .flatten()
            .and_then(|(_, fragment)| {
                percent_encoding::percent_decode_str(fragment)
                    .decode_utf8()
                    .ok()
            })
            .filter(|fragment| !fragment.is_empty());
        let local_source = self.view == WorkspaceView::Editor
            && self
                .active_file()
                .is_some_and(|file| file.path.is_absolute());
        let path = Self::language_source_path(self.repository.as_deref(), uri, local_source);
        let Some(path) = path else {
            toast::info(window, cx, t!("workspace.external_definition"));
            return;
        };
        if markdown_source
            && self
                .standalone
                .as_ref()
                .is_some_and(|current| current != &path)
        {
            self.open_local_window(path, fragment.map(|value| value.into_owned()), window, cx);
            return;
        }
        if self.view == WorkspaceView::Git {
            self.switch_view(WorkspaceView::Editor, window, cx);
        }

        let position = fragment
            .and_then(|fragment| {
                if docs::is_markdown(&path) {
                    Some(SourcePosition::MarkdownFragment(fragment.into_owned()))
                } else {
                    docs::line_fragment(&fragment)
                        .map(|line| SourcePosition::Lsp(lsp_types::Position::new(line, 0)))
                }
            })
            .or_else(|| {
                params
                    .selection
                    .map(|range| SourcePosition::Lsp(range.start))
            });
        self.request_file(
            path,
            FileOpen::Select {
                preview: false,
                position,
            },
            window,
            cx,
        );
    }

    fn editor_changed(
        &mut self,
        editor: Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, file)) = self
            .files
            .iter_mut()
            .find(|(_, file)| file.editor == editor)
        else {
            return;
        };
        let text = editor.read(cx).text();
        if &file.committed == text {
            return;
        }
        file.committed = text.clone();
        if self.standalone.is_some() {
            file.dirty = file.committed != file.saved;
            file.revision += 1;
            file.pending_draft = true;
            self.schedule_draft_write(window, cx);
            cx.notify();
            return;
        }
        file.git.clear_hunk(&editor, cx);
        file.revision += 1;
        file.pending_draft = true;
        let changed_local_doc = path.is_absolute()
            && self
                .doc
                .as_ref()
                .is_some_and(|doc| doc.read(cx).path == *path);
        if let Some(panel) = &self.git {
            let text = file.committed.clone();
            let saved = text == file.saved;
            panel.update(cx, |panel, cx| {
                panel.refresh_edited_source_inlay_hints(path, &text, cx);
                if saved {
                    panel.schedule_snapshot_alignment(cx);
                }
                if self.view == WorkspaceView::Git {
                    panel.buffer_changed(path, text, saved, window, cx);
                }
            });
        }
        let active = self.tabs.active() == Some(path);
        let path = path.clone();
        self.tabs.keep(&path);
        if active {
            self.reveal_active_tab();
        }
        if changed_local_doc {
            self.capture_document_position(cx);
            self.doc = None;
        }
        if active && self.view == WorkspaceView::Editor {
            self.refresh_editor_git_marks(&path, window, cx);
        }
        self.schedule_draft_write(window, cx);
        cx.notify();
    }

    fn schedule_draft_write(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft_task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(220))
                .await;
            _ = this.update_in(cx, |this, window, cx| this.queue_drafts(window, cx));
        });
    }

    fn flush_before_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.saving.is_empty() {
            return false;
        }
        match self.flush_drafts(cx).and_then(|()| self.flush_session(cx)) {
            Ok(()) => {
                self.shutdown_terminals(cx);
                true
            }
            Err(error) => {
                self.show_error(error, window, cx);
                cx.notify();
                false
            }
        }
    }

    fn save(&mut self, _: &SaveFile, window: &mut Window, cx: &mut Context<Self>) {
        if self.repository_ui.picker_open
            || self.repository_ui.opening.is_some()
            || self.confirming_close
        {
            return;
        }
        if self.view == WorkspaceView::Git {
            let path = self
                .git
                .as_ref()
                .and_then(|panel| panel.read(cx).editing_path(window, cx));
            if let Some(path) = path {
                self.start_save(path, false, window, cx);
            }
            return;
        }
        if self.view == WorkspaceView::Editor
            && let Some(path) = self.tabs.active().map(Path::to_owned)
        {
            self.keep_tab(&path, cx);
            self.start_save(path, false, window, cx);
        }
    }

    fn start_save(
        &mut self,
        path: PathBuf,
        close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree_busy || self.saving.contains(&path) || self.git_edit.contains(&path) {
            return;
        }
        if self
            .files
            .get(&path)
            .is_some_and(|file| file.document.conflict || file.missing)
        {
            self.prompt_conflict(path, window, cx);
            return;
        }
        let repo = self.repository.clone();
        if repo.is_none() && self.standalone.is_none() {
            return;
        }
        if let Err(error) = self.flush_drafts(cx) {
            self.show_error(error, window, cx);
            cx.notify();
            return;
        }
        let buffer = self
            .files
            .get(&path)
            .map(|file| (file.document.clone(), file.committed.clone(), file.dirty));
        let store = self.store.clone();
        let target = path.clone();
        let task = cx.background_spawn(async move {
            let (mut document, text, dirty) = match buffer {
                Some(buffer) => buffer,
                None => {
                    let repo = repo
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Missing editor buffer"))?;
                    let document = Document::open(repo, &target, &store)?;
                    let text = Rope::from(document.text.as_str());
                    let dirty = document.text != document.saved_text;
                    (document, text, dirty)
                }
            };
            let result = if dirty {
                document.save_target(repo.as_deref(), &target, &text.to_string())
            } else {
                Ok(())
            };
            Ok::<_, anyhow::Error>((document, text, result))
        });
        self.saving.insert(path.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, move |this, window, cx| {
                this.saving.remove(&path);
                // The write itself detects a file that changed on disk, so the
                // conflict can surface on the very first save. It then belongs
                // to the prompt, not to an error toast next to a dialog.
                let mut conflicted = false;
                let result = result.and_then(|(document, text, result)| {
                    let repo = this.repository.as_ref();
                    if let Some(file) = this.files.get_mut(&path) {
                        file.document = document;
                        conflicted = file.document.conflict;
                        if conflicted {
                            file.editor
                                .update(cx, |editor, cx| editor.set_readonly(true, cx));
                        }
                        if let Err(error) = result {
                            if !path.is_absolute()
                                && repo.is_some_and(|repo| repo.ssh.is_some())
                                && !file.document.conflict
                            {
                                file.unconfirmed_save = Some(text);
                            }
                            return Err(error);
                        }
                        file.unconfirmed_save = None;
                        file.saved = text.clone();
                        file.dirty = file.committed != file.saved;
                        file.pending_draft = true;
                        file.revision += 1;
                        if let Some(server) = &file.language_server {
                            server.document.saved(text);
                        }
                        this.flush_drafts(cx)?;
                    } else {
                        result?;
                        if let Some(repo) = repo {
                            this.store.discard_draft(repo, &path)?;
                        }
                    }
                    Ok(())
                });
                if conflicted {
                    if this.tabs.active() == Some(&path) {
                        this.language_format = Task::ready(());
                    }
                    if close && this.confirming_close {
                        // The close was already being confirmed. A conflict is a
                        // new decision, so drop the pending close rather than
                        // stacking a second dialog on top of it.
                        this.cancel_tab_close(window, cx);
                        window.close_dialog(cx);
                    }
                    this.prompt_conflict(path, window, cx);
                } else {
                    match result {
                        Ok(()) => {
                            if !path.is_absolute() {
                                this.refresh_git_after_change(&path, window, cx);
                            }
                            if this.standalone.is_some()
                                && (this
                                    .doc
                                    .as_ref()
                                    .is_some_and(|doc| doc.read(cx).path == path)
                                    || this
                                        .doc_loading
                                        .as_ref()
                                        .is_some_and(|doc| doc.path == path))
                            {
                                this.capture_document_position(cx);
                                this.doc = None;
                                this.doc_task = Task::ready(());
                                this.doc_loading = None;
                                this.doc_error = None;
                                if this.view == WorkspaceView::Docs {
                                    this.open_doc(path.clone(), None, window, cx);
                                }
                            } else if this
                                .doc
                                .as_ref()
                                .is_some_and(|doc| doc.read(cx).path == path)
                            {
                                this.capture_document_position(cx);
                                this.doc = None;
                            }
                            if close && this.confirming_close && !this.tab_is_dirty(&path) {
                                this.finish_tab_close(&path, window, cx);
                            }
                        }
                        Err(error) => this.show_error(error, window, cx),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Ask what to do with a draft that can no longer be saved over the file.
    ///
    /// The conflict is a state, not an event: the tab badge marks it, and this
    /// prompt is the only message it gets. Saving therefore never answers with
    /// an error toast next to a dialog, and the first save already asks, even
    /// when the write itself is what discovered the change on disk.
    fn prompt_conflict(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let reload = workspace.clone();
            let reload_path = path.clone();
            dialog
                .entrance(gpui_component::dialog::DialogEntrance::Fade)
                .centered()
                .width(px(480.))
                .close_button(false)
                .title(t!("workspace.conflict_title"))
                .child(div().child(paths::display(&path)))
                .child(div().text_sm().child(t!("workspace.draft_conflict")))
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            action_button("keep-draft")
                                .secondary()
                                .label(t!("workspace.keep_draft"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            action_button("reload-from-disk")
                                .primary()
                                .label(t!("workspace.reload_from_disk"))
                                .on_click(move |_, window, cx| {
                                    _ = reload.update(cx, |this, cx| {
                                        this.reload_from_disk(&reload_path, window, cx)
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    /// Drop the draft for `path` and load what is on disk now.
    fn reload_from_disk(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving.contains(path) {
            return;
        }
        let repo = self.repository.clone();
        if repo.is_none() && self.standalone.is_none() {
            return;
        }
        let store = self.store.clone();
        let target = path.to_owned();
        self.saving.insert(target.clone());
        cx.spawn_in(window, async move |this, cx| {
            let read_path = target.clone();
            let result = cx
                .background_spawn(async move {
                    if let Some(repo) = repo {
                        let document = Document::read(&repo, &read_path)?;
                        store.discard_draft(&repo, &read_path)?;
                        Ok::<_, anyhow::Error>(document)
                    } else {
                        let document = Document::read_local(&read_path)?;
                        store.discard_local_editor_draft(&read_path)?;
                        Ok(document)
                    }
                })
                .await;
            _ = this.update_in(cx, |this, window, cx| {
                this.saving.remove(&target);
                let document = match result {
                    Ok(document) => document,
                    Err(error) => {
                        toast::error(window, cx, format!("{error:#}"));
                        return;
                    }
                };
                let Some(file) = this.files.get_mut(&target) else {
                    return;
                };
                let text = document.saved_text.clone();
                file.saved = Rope::from(text.as_str());
                file.committed = file.saved.clone();
                file.document = document;
                file.unconfirmed_save = None;
                file.dirty = false;
                file.pending_draft = this.standalone.is_none();
                file.revision += 1;
                file.editor.update(cx, |editor, cx| {
                    editor.set_value(text.clone(), window, cx);
                    editor.set_readonly(false, cx);
                });
                if this.standalone.is_some()
                    && this
                        .doc
                        .as_ref()
                        .is_some_and(|doc| doc.read(cx).path == target)
                {
                    this.capture_document_position(cx);
                    this.doc = None;
                    if this.view == WorkspaceView::Docs {
                        this.open_doc(target.clone(), None, window, cx);
                    }
                }
                if let Some(server) = this
                    .files
                    .get(&target)
                    .and_then(|file| file.language_server.as_ref())
                {
                    server.document.changed(Rope::from(text.as_str()));
                    server.document.saved(Rope::from(text.as_str()));
                }
                if let Some(panel) = &this.git {
                    panel.update(cx, |panel, cx| panel.repository_source_changed(target, cx));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn toggle_wrap(&mut self, _: &ToggleWrap, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == WorkspaceView::Git {
            if let Some(panel) = &self.git {
                panel.update(cx, |panel, cx| panel.toggle_wrap(window, cx));
            }
            return;
        }
        self.wrap = !self.wrap;
        if self.view == WorkspaceView::Editor
            && let Some(file) = self.tabs.active().and_then(|path| self.files.get_mut(path))
        {
            file.apply_wrap(self.wrap, window, cx);
        }
        cx.notify();
    }

    fn active_file(&self) -> Option<&OpenFile> {
        self.tabs.active().and_then(|path| self.files.get(path))
    }

    fn file_editor_locked(&self, file: &OpenFile) -> bool {
        file.missing
            || file.document.conflict
            || file.document.mixed_newlines
            || self.git_edit.contains(&file.path)
    }

    fn tab_is_dirty(&self, path: &Path) -> bool {
        if let Some(file) = self.files.get(path) {
            file.is_dirty()
        } else {
            self.repository
                .as_ref()
                .is_some_and(|repo| self.store.has_draft(repo, path))
        }
    }

    /// Whether the tab needs attention because its disk state changed.
    fn tab_needs_attention(&self, path: &Path) -> bool {
        self.files
            .get(path)
            .is_some_and(|file| file.document.conflict || file.missing)
    }

    fn activate_tab(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.index(path).is_some() {
            self.open_file(path.to_owned(), true, window, cx);
        }
    }

    fn finish_tab_selection(
        &mut self,
        previous: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.tabs.active();
        let changed = active != previous;
        if changed {
            self.language_rename = Task::ready(());
            self.language_format = Task::ready(());
            self.lsp_panel_open = false;
            if let Some(file) = previous.and_then(|path| self.files.get(path)) {
                file.editor
                    .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
            }
        }
        if self.view == WorkspaceView::Editor && !self.confirming_close {
            let keep_focus = self.locator.open
                || self.repository_overlay_open()
                || self.terminal_focused(window, cx)
                || (self.tree_focus.is_focused(window)
                    && active == Some(self.tree_keyboard_path.as_path()));
            if let Some(file) = active.and_then(|path| self.files.get_mut(path)) {
                file.apply_wrap(self.wrap, window, cx);
                if !keep_focus {
                    file.editor
                        .update(cx, |editor, cx| editor.focus(window, cx));
                }
            } else if !keep_focus {
                self.focus.focus(window, cx);
            }
            if active.is_some() {
                self.reveal_active_tab();
            }
            self.reveal_file_in_tree(window, cx);
            self.watch_editor_git(window, cx);
        }
        cx.notify();
    }

    fn reveal_active_tab(&self) {
        if let Some(index) = self.tabs.active().and_then(|path| self.tabs.index(path)) {
            self.tab_scroll.scroll_to_item(index);
        }
    }

    fn keep_tab(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.tabs.keep(path) {
            cx.notify();
        }
    }

    fn cycle_tabs(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.active().and_then(|path| self.tabs.index(path)) {
            let count = self.tabs.paths().len();
            let next = if forward {
                (index + 1) % count
            } else {
                (index + count - 1) % count
            };
            let path = self.tabs.paths()[next].clone();
            self.activate_tab(&path, window, cx);
        }
    }

    fn close_active_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.tabs.active().map(Path::to_owned) {
            self.close_tabs(vec![path], window, cx);
        }
    }

    fn show_error(&mut self, error: anyhow::Error, window: &mut Window, cx: &mut Context<Self>) {
        toast::error(window, cx, format!("{error:#}"));
    }

    fn close_tabs(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming_close || self.repository_ui.opening.is_some() {
            return;
        }
        if self
            .loading_file
            .as_ref()
            .is_some_and(|(path, _)| paths.contains(path))
        {
            self.cancel_file_open();
        }
        self.close_queue = paths
            .into_iter()
            .filter(|path| !self.saving.contains(path))
            .collect();
        let previous = self.tabs.active().map(Path::to_owned);
        self.drain_close_queue(previous.as_deref(), window, cx);
    }

    fn drain_close_queue(
        &mut self,
        previous: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut pending = None;
        while let Some(path) = self.close_queue.pop_front() {
            if self.tab_is_dirty(&path) {
                pending = Some(path);
                break;
            }
            self.remove_tab(&path, window, cx);
        }
        if let Some(path) = pending {
            self.confirm_tab_close(path, window, cx);
        } else if std::mem::take(&mut self.closing_standalone_window) {
            if self.request_window_close(window, cx) {
                window.remove_window();
                return;
            }
        } else if previous != self.tabs.active()
            || (self.active_file().is_none() && self.active_image_tab().is_none())
        {
            self.restore_active_file(window, cx);
        }
        if self.tabs.paths().is_empty() {
            self.open_files_list = false;
        }
        self.reveal_active_tab();
        cx.notify();
    }

    fn remove_tab(&mut self, path: &Path, window: &mut Window, cx: &App) {
        if self.tabs.active() == Some(path) {
            self.language_format = Task::ready(());
            self.capture_active_file_position(cx);
        }
        self.tabs.close(path);
        self.files.remove(path);
        self.remove_image_tab(path, window);
    }

    fn confirm_tab_close(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_file_open();
        self.confirming_close = true;
        if let Some(file) = self.active_file() {
            file.editor
                .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
        }
        self.open_files_list = false;
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let locked = workspace
                .read_with(cx, |this, _| {
                    this.files
                        .get(&path)
                        .is_some_and(|file| file.document.conflict || file.document.mixed_newlines)
                })
                .unwrap_or_default();
            let cancel = workspace.clone();
            let discard = workspace.clone();
            let save = workspace.clone();
            let discard_path = path.clone();
            let save_path = path.clone();
            dialog
                .entrance(gpui_component::dialog::DialogEntrance::Fade)
                .centered()
                .width(px(480.))
                .close_button(false)
                .title(t!("workspace.unsaved_changes"))
                .on_cancel(move |_, window, cx| {
                    _ = cancel.update(cx, |this, cx| this.cancel_tab_close(window, cx));
                    true
                })
                .child(div().child(paths::display(&path)))
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            action_button("cancel-tab-close")
                                .secondary()
                                .label(t!("common.cancel"))
                                .on_click({
                                    let workspace = workspace.clone();
                                    move |_, window, cx| {
                                        _ = workspace.update(cx, |this, cx| {
                                            this.cancel_tab_close(window, cx)
                                        });
                                        window.close_dialog(cx);
                                    }
                                }),
                        )
                        .child(
                            action_button("discard-tab")
                                .danger()
                                .label(t!("workspace.discard_changes"))
                                .on_click(move |_, window, cx| {
                                    _ = discard.update(cx, |this, cx| {
                                        this.resolve_tab_close(&discard_path, false, window, cx)
                                    });
                                }),
                        )
                        .child(
                            action_button("save-and-close-tab")
                                .primary()
                                .disabled(locked)
                                .label(t!("workspace.save_and_close"))
                                .on_click(move |_, window, cx| {
                                    _ = save.update(cx, |this, cx| {
                                        this.resolve_tab_close(&save_path, true, window, cx)
                                    });
                                }),
                        ),
                )
        });
    }

    fn cancel_tab_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming_close = false;
        self.closing_standalone_window = false;
        self.close_queue.clear();
        if self.active_file().is_none() && self.active_image_tab().is_none() {
            self.restore_active_file(window, cx);
        }
        cx.notify();
    }

    fn resolve_tab_close(
        &mut self,
        path: &Path,
        save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving.contains(path) {
            return;
        }
        if save {
            self.start_save(path.to_owned(), true, window, cx);
            return;
        }
        let result = {
            let mut draft = self
                .files
                .get(path)
                .map(|file| (file.revision, file.draft_write.lock().unwrap()));
            let result = self.repository.as_ref().map_or_else(
                || self.store.discard_local_editor_draft(path),
                |repo| self.store.discard_draft(repo, path),
            );
            if result.is_ok()
                && let Some((revision, write)) = draft.as_mut()
            {
                write.revision = *revision;
                write.result = None;
            }
            result
        };
        if let Err(error) = result {
            self.show_error(error, window, cx);
            cx.notify();
            return;
        }
        self.finish_tab_close(path, window, cx);
    }

    fn finish_tab_close(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming_close = false;
        window.close_dialog(cx);
        let previous = self.tabs.active().map(Path::to_owned);
        self.remove_tab(path, window, cx);
        self.drain_close_queue(previous.as_deref(), window, cx);
    }

    fn topbar(&self, window: &Window, cx: &Context<Self>, colors: Palette) -> impl IntoElement {
        let nav = h_flex()
            .gap(px(4.))
            .p(px(3.))
            .rounded(px(8.))
            .bg(colors.nav_track)
            .child(
                nav_item(
                    "nav-docs",
                    t!("common.docs"),
                    "Markdown",
                    self.view == WorkspaceView::Docs,
                    window,
                    cx,
                    colors,
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.switch_view(WorkspaceView::Docs, window, cx)
                })),
            )
            .child(
                nav_item(
                    "nav-workspace",
                    t!("common.workspace"),
                    t!("workspace.code"),
                    self.view == WorkspaceView::Editor,
                    window,
                    cx,
                    colors,
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.switch_view(WorkspaceView::Editor, window, cx)
                })),
            )
            .child(
                nav_item(
                    "nav-git",
                    "Git",
                    t!("git.history"),
                    self.view == WorkspaceView::Git,
                    window,
                    cx,
                    colors,
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.switch_view(WorkspaceView::Git, window, cx)
                })),
            );
        TitleBar::new()
            .on_close_window(cx.listener(|this, _, window, cx| {
                if this.request_window_close(window, cx) {
                    window.remove_window();
                }
            }))
            .h(px(36.))
            .bg(colors.titlebar)
            .border_0()
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .pr_2()
                    .child(self.repository_trigger(window, cx, colors))
                    .child(div().flex_1())
                    .when(self.repository.is_some(), |bar| bar.child(nav))
                    .child(div().flex_1())
                    .child(
                        titlebar_actions()
                            .gap(px(4.))
                            .when(self.repository.is_some(), |tools| {
                                tools
                                    .child(
                                        icon_button(
                                            "search",
                                            IconName::Search,
                                            t!(
                                                "locator.shortcut",
                                                keys = if cfg!(target_os = "macos") {
                                                    "⌘P"
                                                } else {
                                                    "Ctrl+P"
                                                }
                                            )
                                            .into_owned(),
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.open_locator(window, cx)
                                            }),
                                        ),
                                    )
                                    .child(self.theme_button(cx))
                            })
                            .child(
                                icon_button(
                                    "open-settings",
                                    IconName::Settings,
                                    t!("common.settings"),
                                )
                                .when(self.config.load_error.is_some(), |button| {
                                    button.tooltip(t!("workspace.settings_read_failed"))
                                })
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.toggle_settings(&ToggleSettings, window, cx)
                                    },
                                )),
                            ),
                    ),
            )
    }

    pub(super) fn theme_button(&self, cx: &Context<Self>) -> Button {
        let theme = self.config.current().appearance.theme;
        icon_button(
            "theme",
            theme.icon(),
            t!("settings.theme_label", theme = theme.label()).into_owned(),
        )
        .tooltip(
            t!(
                "settings.theme_switch",
                current = theme.label(),
                next = theme.next().label()
            )
            .into_owned(),
        )
        .on_click(cx.listener(Self::cycle_theme))
    }

    fn find(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == WorkspaceView::Git {
            if let Some(panel) = &self.git {
                panel.update(cx, |panel, cx| panel.find(window, cx));
            }
        } else if self.view == WorkspaceView::Docs {
            if let Some(doc) = &self.doc {
                doc.update(cx, |doc, cx| doc.toggle_find(window, cx));
            }
        } else if let Some(file) = self.active_file() {
            file.editor
                .update(cx, |editor, cx| editor.focus(window, cx));
            window.dispatch_action(Box::new(Search), cx);
        }
    }

    fn sidebar(
        &self,
        width: Pixels,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let rows = self.rows.clone();
        let in_workspace = self.view == WorkspaceView::Editor;
        let drag_repo = self
            .tree_transfer_available()
            .then(|| self.repository.as_ref().unwrap().clone());
        let status = self.tree_status.clone();
        let inline = self.tree_edit.as_ref().and_then(|edit| {
            let (index, depth, replace) = edit.row(&rows)?;
            let guides = if replace {
                rows[index].guides
            } else if depth == 0 {
                file_tree::TreeGuides::default()
            } else {
                let has_children = rows.get(index).is_some_and(|row| row.depth == depth);
                file_tree::TreeGuides::new(
                    rows[index - 1].guides.continuing(),
                    depth,
                    !has_children,
                )
            };
            Some((index, depth, replace, guides))
        });
        let row_count = rows.len() + usize::from(inline.is_some_and(|(_, _, replace, _)| !replace));
        let active = if self.view == WorkspaceView::Docs {
            self.session.document.clone()
        } else if self.tree_focus.is_focused(window) {
            Some(self.tree_keyboard_path.clone())
        } else {
            self.tabs.active().map(Path::to_owned)
        };
        let weak = cx.entity().downgrade();
        let root_menu = weak.clone();
        let frame = self.tree_motion.borrow_mut().frame(
            &rows,
            |row| row.entry.path.clone(),
            inline.is_none(),
            window,
            cx,
        );
        let rows = frame.as_ref().map_or(rows, |frame| frame.rows.clone());
        let list = tree_motion::tree_list(
            cx.entity(),
            "file-tree",
            row_count,
            &self.tree_scroll,
            frame,
            move |index, _, cx| {
                let mut row_index = index;
                if let Some((edit_index, depth, replace, guides)) = inline {
                    if index == edit_index {
                        return weak
                            .read_with(cx, |this, _| {
                                this.tree_edit.as_ref().map(|edit| {
                                    edit.element(
                                        depth,
                                        guides,
                                        colors.muted,
                                        this.tree_busy,
                                        weak.clone(),
                                    )
                                })
                            })
                            .ok()
                            .flatten();
                    }
                    if index > edit_index && !replace {
                        row_index -= 1;
                    }
                }
                let row = &rows[row_index];
                let entry = row.entry.clone();
                let directory = entry.directory;
                let selected = active.as_ref() == Some(&entry.path);
                let view = weak.clone();
                let clicked = entry.clone();
                let row_id = format!("workspace-tree-row:{}", paths::display(&entry.path));
                let name = file_name(&entry.path).to_owned();
                let decoration = in_workspace
                    .then(|| status.decoration(&entry, colors, cx))
                    .flatten();
                let icon_color = if selected {
                    colors.accent
                } else {
                    colors.muted
                };
                let entry_icon = if directory {
                    folder_icons::folder_icon(&entry.path, row.expanded, icon_color, colors)
                } else if entry.symlink {
                    icon(IconName::Link, icon_color, 15.).into_any_element()
                } else {
                    file_type_icon(&entry.path, colors, 15.).into_any_element()
                };
                let element = h_flex()
                    .id(SharedString::from(row_id.clone()))
                    .debug_selector(move || row_id.clone())
                    .relative()
                    .h(tree_motion::ROW_HEIGHT)
                    .w_full()
                    .gap(px(5.))
                    .pl(file_tree::tree_indent(row.depth))
                    .pr_2()
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.hover))
                    .when(selected, |row| {
                        row.bg(colors.accent.opacity(0.15))
                            .text_color(colors.accent)
                            .font_semibold()
                    })
                    .when(
                        in_workspace
                            && !directory
                            && !entry.symlink
                            && audio_preview::is_audio(&entry.path),
                        |row| {
                            row.on_mouse_down(MouseButton::Left, |_, window, _| {
                                window.prevent_default()
                            })
                        },
                    )
                    .on_click(move |event, window, cx| {
                        _ = view.update(cx, |this, cx| {
                            this.tree_click(clicked.clone(), event, window, cx)
                        });
                    })
                    .children(row.guides.element(colors.muted))
                    .child(entry_icon)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(file_name_font_size(cx)))
                            .when(in_workspace && row.dimmed && !selected, |name| {
                                name.text_color(colors.muted)
                            })
                            .when_some(decoration.as_ref(), |name, (color, _)| {
                                name.text_color(*color)
                            })
                            .child(name),
                    );
                // Native tooltips lay out at min-content and collapse this row's
                // clamped content into a vertical strip; use the window-managed tooltip.
                let element = element.when_some(decoration, |row, (_, label)| {
                    use gpui_component::tooltip::{ManagedTooltipExt as _, Tooltip};
                    row.managed_tooltip(move |window, cx| {
                        Tooltip::new(label.clone()).build(window, cx)
                    })
                });
                Some(if in_workspace {
                    let element =
                        tree_transfer::tree_drag_source(element, &entry, drag_repo.as_ref());
                    let element = if entry.directory {
                        tree_transfer::tree_drop_target(
                            element,
                            entry.path.clone(),
                            weak.clone(),
                            colors,
                        )
                    } else {
                        element
                    };
                    file_tree::tree_menu(element, Some(entry), weak.clone())
                } else {
                    file_tree::document_tree_menu(element, Some(entry), weak.clone())
                })
            },
        );
        v_flex()
            .id("workspace-file-tree-pane")
            .relative()
            .w(width)
            .h_full()
            .flex_shrink_0()
            .when(in_workspace, |pane| {
                pane.track_focus(&self.tree_focus)
                    .key_context("WorkspaceFileTree")
                    .on_drop(|_: &ExternalPaths, _, _| {})
                    .on_action(
                        cx.listener(|this, _: &tree_transfer::CopyTreeEntry, window, cx| {
                            this.tree_key_copy(false, window, cx);
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &tree_transfer::CutTreeEntry, window, cx| {
                            this.tree_key_copy(true, window, cx);
                        }),
                    )
                    .on_action(cx.listener(
                        |this, _: &tree_transfer::PasteTreeEntry, window, cx| {
                            this.tree_key_paste(false, window, cx);
                        },
                    ))
                    .on_action(cx.listener(
                        |this, _: &tree_transfer::MoveTreeClipboard, window, cx| {
                            this.tree_key_paste(true, window, cx);
                        },
                    ))
            })
            .child({
                let header = h_flex()
                    .id("workspace-tree-header")
                    .h(px(40.))
                    .px_3()
                    .child(div().flex_1().font_semibold().text_size(px(12.5)).child(
                        if self.view == WorkspaceView::Docs {
                            t!("common.docs")
                        } else {
                            t!("common.workspace")
                        },
                    ))
                    .when(
                        self.view == WorkspaceView::Docs
                            && self.docs_tree_loading
                            && !self.docs_tree_complete,
                        |header| {
                            header.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(colors.muted)
                                    .child(t!("docs.loading_tree")),
                            )
                        },
                    )
                    .child(
                        icon_button(
                            "collapse",
                            IconName::ListCollapse,
                            t!("workspace.collapse_all"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.tree_motion.get_mut().start();
                            if this.view == WorkspaceView::Docs {
                                this.docs_expanded.clear();
                            } else {
                                this.expanded.clear();
                                this.tree_reveal = None;
                            }
                            this.rebuild_tree();
                            cx.notify();
                        })),
                    )
                    .child(
                        icon_button(
                            "hide-file-tree",
                            IconName::PanelLeftClose,
                            t!("workspace.hide_file_tree"),
                        )
                        .debug_selector(|| "hide-file-tree".into())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_file_tree_visible(false);
                            cx.notify();
                        })),
                    );
                if in_workspace {
                    let header = tree_transfer::tree_drop_target(
                        header.on_click(cx.listener(|this, _, window, cx| {
                            this.focus_tree(PathBuf::new(), window, cx);
                        })),
                        PathBuf::new(),
                        root_menu.clone(),
                        colors,
                    );
                    file_tree::tree_menu(header, None, root_menu.clone())
                } else {
                    file_tree::document_tree_menu(header, None, root_menu.clone())
                }
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(if in_workspace {
                        file_tree::tree_menu(
                            div().id("workspace-tree-root-menu").size_full().child(list),
                            None,
                            root_menu,
                        )
                    } else {
                        list.into_any_element()
                    })
                    .child(
                        div().absolute().inset_0().child(
                            pane_scrollbar(&self.tree_scroll)
                                .id("workspace-file-tree-scroll")
                                .viewport_from_layout(),
                        ),
                    ),
            )
            .child(panel_resize(
                "workspace-tree-resize",
                HandleEdge::Trailing,
                FILE_TOOLBAR_HEIGHT,
            ))
            .child(pane_edge(
                HandleEdge::Trailing,
                FILE_TOOLBAR_HEIGHT,
                cx.theme().mode,
            ))
    }

    fn editor_font_family(&self, cx: &App) -> SharedString {
        self.editor_font
            .clone()
            .unwrap_or_else(|| cx.theme().mono_font_family.clone())
    }

    fn content(&self, window: &Window, cx: &Context<Self>, colors: Palette) -> Stateful<Div> {
        let editor_background =
            code_theme::surface(self.config.current().appearance.code_theme, cx.theme().mode);
        let maximum = ((window.viewport_size().height - px(68.)) * 0.65).max(px(100.));
        let mut terminal = self.terminal_panel(maximum, colors, cx);
        let maximized = self.terminal_maximized();
        let mut main = v_flex().relative().flex_1().min_w_0().min_h_0().h_full();
        if let Some((path, image)) = self.active_image_tab() {
            main = main
                .when(self.standalone.is_none(), |main| {
                    main.child(self.tab_bar(cx, colors))
                })
                .child(self.image_tab_content(path, image, cx, colors));
        } else if let Some(file) = self.active_file() {
            let path = &file.path;
            let name = file_name(path).to_owned();
            let language_server = file
                .language_server
                .as_ref()
                .filter(|_| !file.missing)
                .map(|server| Arc::downgrade(&server.document));
            let locked = self.file_editor_locked(file);
            main = main
                .when(self.standalone.is_none(), |main| {
                    main.child(self.tab_bar(cx, colors))
                })
                .child(
                    editor_scroll_viewport(&file.editor, editor_background, !self.locator.open)
                        .debug_selector(|| "workspace-editor".into())
                        .capture_any_mouse_down(cx.listener(Self::begin_source_click))
                        .on_mouse_up(MouseButton::Left, cx.listener(Self::end_source_click))
                        .on_mouse_up_out(MouseButton::Left, cx.listener(Self::end_source_click))
                        .on_action(cx.listener(Self::close_editor_git_hunk))
                        .on_action(cx.listener(Self::show_signature_help))
                        .capture_key_down(cx.listener(Self::dismiss_signature_help))
                        .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                            if !action.secondary && !action.shift {
                                this.block_pending_completion_acceptance(window, cx);
                            }
                        }))
                        .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                            this.block_pending_completion_acceptance(window, cx);
                        }))
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .child(
                            Editor::new(&file.editor)
                                .text_size(px(self.config.current().editor.font_size as f32))
                                .line_height(relative(
                                    self.config.current().editor.line_height as f32,
                                ))
                                .font_family(self.editor_font_family(cx))
                                .appearance(false)
                                .bordered(false)
                                .readonly(locked)
                                .size_full()
                                .aria_label(name)
                                .when_some(language_server, |editor, document| {
                                    editor.extend_context_menu(move |menu, _, _| {
                                        if document
                                            .upgrade()
                                            .is_some_and(|document| document.supports_references())
                                        {
                                            menu.menu_with_icon(
                                                t!("lsp.find_references"),
                                                IconName::Search,
                                                Box::new(FindReferences),
                                            )
                                        } else {
                                            menu
                                        }
                                    })
                                }),
                        )
                        .children(file.caret_hints.popup(&file.editor, window, cx))
                        .when(!maximized, |pane| {
                            pane.children(editor_git::blame::hint(
                                file,
                                cx.weak_entity(),
                                px(self.config.current().editor.font_size as f32),
                                self.editor_font_family(cx),
                                colors,
                                cx,
                            ))
                        })
                        .when(file.missing, |pane| {
                            pane.child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .right_0()
                                    .px_2()
                                    .text_sm()
                                    .text_color(colors.warning)
                                    .child(t!("workspace.tree_file_missing")),
                            )
                        }),
                )
                .children(if maximized { None } else { terminal.take() });
        } else {
            main = main
                .when(self.standalone.is_none(), |main| {
                    main.child(
                        h_flex()
                            .justify_end()
                            .h(FILE_TOOLBAR_HEIGHT)
                            .w_full()
                            .bg(colors.bg)
                            .px_2()
                            .children(self.show_file_tree_button(cx))
                            .child(div().flex_1())
                            .child(self.workspace_actions(cx)),
                    )
                })
                .child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .gap_3()
                        .items_center()
                        .justify_center()
                        .text_color(colors.muted)
                        .child(icon(IconName::FolderOpen, colors.accent, 30.))
                        .child(if self.loading_file.is_some() {
                            t!("workspace.opening_file")
                        } else {
                            t!("workspace.select_file")
                        }),
                );
        }
        self.terminal_column(
            if maximized {
                v_flex()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .child(main.invisible())
                    .children(terminal)
            } else {
                main.children(terminal)
            },
            maximum,
            cx,
        )
    }

    fn status_bar(
        &self,
        file: &OpenFile,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let path = &file.path;
        let radii = window_corner_radii(window, cx);
        let locked =
            file.document.conflict || file.document.mixed_newlines || self.git_edit.contains(path);
        let status_size = 11.;
        let progress = self.language_service_progress(file, status_size, colors, cx);
        // Every text/button in this row goes through `baseline_aligned`,
        // so new status content with a different size still shares the
        // row's baseline without forcing one font size everywhere.
        h_flex()
            .h(px(26.))
            .flex_shrink_0()
            .bg(colors.soft)
            .rounded_bl(radii.bottom_left)
            .rounded_br(radii.bottom_right)
            .px_3()
            .gap_3()
            .text_size(px(11.))
            .text_color(colors.muted)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(
                        baseline_aligned(
                            cx,
                            status_size,
                            status_size,
                            div().truncate().child(paths::display(path)),
                        )
                        .min_w_0()
                        .debug_selector(|| "status-file-path".into()),
                    )
                    .children(progress),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_3()
                    .child(
                        baseline_aligned(cx, status_size, status_size, language_for(path))
                            .debug_selector(|| "status-file-language".into()),
                    )
                    .child(
                        baseline_aligned(cx, status_size, status_size, "UTF-8")
                            .debug_selector(|| "status-file-encoding".into()),
                    )
                    .when(self.loading_file.is_some(), |bar| {
                        bar.child(baseline_aligned(
                            cx,
                            status_size,
                            status_size,
                            t!("workspace.opening_file"),
                        ))
                    })
                    .when(locked, |bar| {
                        bar.child(baseline_aligned(
                            cx,
                            status_size,
                            status_size,
                            div()
                                .text_color(colors.warning)
                                .child(t!("workspace.read_only")),
                        ))
                    })
                    .child(
                        h_flex()
                            .debug_selector(|| "status-file-controls".into())
                            .gap(px(4.))
                            .child(
                                baseline_aligned(
                                    cx,
                                    status_size,
                                    status_size,
                                    status_button("editor-wrap")
                                        .w(px(58.))
                                        .toggled(file.wrap)
                                        .label(if file.wrap {
                                            t!("workspace.wrap_on")
                                        } else {
                                            t!("workspace.wrap_off")
                                        })
                                        .tooltip(if file.wrap {
                                            t!("workspace.disable_wrap")
                                        } else {
                                            t!("workspace.enable_wrap")
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.toggle_wrap(&ToggleWrap, window, cx);
                                        })),
                                )
                                .flex_shrink_0(),
                            )
                            .when(
                                self.standalone.is_none() || file.server_project.is_some(),
                                |bar| {
                                    bar.child(
                                        baseline_aligned(
                                            cx,
                                            status_size,
                                            status_size,
                                            self.language_service_status(file, colors, cx),
                                        )
                                        .flex_shrink_0(),
                                    )
                                },
                            ),
                    ),
            )
    }

    fn show_file_tree_button(&self, cx: &Context<Self>) -> Option<Button> {
        let visible = if self.view == WorkspaceView::Docs {
            self.session.panels.docs_tree
        } else {
            self.session.panels.editor_tree
        };
        (!visible && self.standalone.is_none()).then(|| {
            icon_button(
                "show-file-tree",
                IconName::PanelLeftOpen,
                t!("workspace.show_file_tree"),
            )
            .debug_selector(|| "show-file-tree".into())
            .on_click(cx.listener(|this, _, window, cx| {
                this.set_file_tree_visible(true);
                if this.view == WorkspaceView::Editor {
                    this.resume_tree_reveal(window, cx);
                }
                cx.notify();
            }))
        })
    }

    fn set_file_tree_visible(&mut self, visible: bool) {
        let state = if self.view == WorkspaceView::Docs {
            &mut self.session.panels.docs_tree
        } else {
            &mut self.session.panels.editor_tree
        };
        if *state != visible {
            *state = visible;
            self.session_revision += 1;
        }
    }

    fn workspace_body(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let docs = self.view == WorkspaceView::Docs;
        let width = window.viewport_size().width;
        let tree_limit = (width * 0.42).max(px(160.));
        let (visible, tree_width) = if docs {
            (self.session.panels.docs_tree, self.docs_tree_width)
        } else {
            (self.session.panels.editor_tree, self.editor_tree_width)
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                h_flex()
                    .id("workspace-panes")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .on_drag_move(cx.listener(
                        move |this, event: &DragMoveEvent<PaneDrag>, _, cx| {
                            if event.drag(cx).edge != HandleEdge::Trailing {
                                return;
                            }
                            let pointer = event.event.position.x
                                - event.bounds.left()
                                - event.drag(cx).offset();
                            if docs {
                                this.docs_tree_width = pointer.clamp(px(160.), tree_limit);
                            } else {
                                this.editor_tree_width = pointer.clamp(px(160.), tree_limit);
                            }
                            cx.notify();
                        },
                    ))
                    .when(self.standalone.is_none() && visible, |row| {
                        row.child(self.sidebar(tree_width.min(tree_limit), window, cx, colors))
                    })
                    .child(if docs {
                        self.file_drop_area(self.docs_content(window, cx, colors), cx, colors)
                            .into_any_element()
                    } else {
                        self.file_drop_area(self.content(window, cx, colors), cx, colors)
                            .into_any_element()
                    }),
            )
            .when(!docs && !self.terminal_maximized(), |body| {
                body.when_some(self.active_file(), |body, file| {
                    body.child(self.status_bar(file, window, cx, colors))
                })
            })
    }

    /// The menu at the right edge of the tab bar.
    ///
    /// Save and Find act on the open file, so they only appear while one is
    /// open: the same trigger labels the empty editor state, where both entries
    /// would do nothing. Entries keep their icon so the menu still reads like
    /// the row of buttons it replaced.
    fn workspace_actions(&self, cx: &Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        let editor = self
            .active_file()
            .map(|file| file.editor.read(cx).focus_handle(cx));
        let standalone = self.standalone.is_some();
        div()
            .debug_selector(|| "workspace-actions-trigger".into())
            .flex_shrink_0()
            .child(
                icon_button(
                    "workspace-actions",
                    IconName::Ellipsis,
                    t!("workspace.actions"),
                )
                .tooltip_placement(Placement::Bottom)
                .dropdown_menu_with_anchor(
                    Anchor::TopRight,
                    move |mut menu, window, _| {
                        let view = view.clone();
                        let action_icon = |name| {
                            Icon::new(name)
                                .with_size(rems(0.75).to_pixels(window.rem_size()) + px(2.))
                        };
                        if let Some(editor) = &editor {
                            menu = menu.action_context(editor.clone());
                            let save = view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(t!("common.save"))
                                    .icon(action_icon(IconName::Save))
                                    .action(Box::new(SaveFile))
                                    .on_click(move |_, window, cx| {
                                        _ = save.update(cx, |this, cx| {
                                            this.save(&SaveFile, window, cx)
                                        });
                                    }),
                            );
                            let find = view.clone();
                            menu = menu.item(
                                PopupMenuItem::new(t!("common.find"))
                                    .icon(action_icon(IconName::Search))
                                    .action(Box::new(Search))
                                    .on_click(move |_, window, _| {
                                        // Let the menu restore its own focus before
                                        // the editor takes it for the search field.
                                        let find = find.clone();
                                        window.on_next_frame(move |window, cx| {
                                            _ = find.update(cx, |this, cx| this.find(window, cx));
                                        });
                                    }),
                            );
                            if !standalone {
                                menu = menu.separator();
                            }
                        }
                        if standalone {
                            return menu;
                        }
                        menu.item(
                            PopupMenuItem::new(t!("workspace.toggle_terminal"))
                                .icon(action_icon(IconName::Terminal))
                                .action(Box::new(ToggleTerminal))
                                .on_click(move |_, window, _| {
                                    // Let the menu restore its own focus before the terminal takes it.
                                    let view = view.clone();
                                    window.on_next_frame(move |window, cx| {
                                        _ = view.update(cx, |this, cx| {
                                            this.toggle_terminal_menu(window, cx)
                                        });
                                    });
                                }),
                        )
                    },
                ),
            )
    }

    fn docs_content(&self, window: &Window, cx: &Context<Self>, colors: Palette) -> Stateful<Div> {
        v_flex()
            .id("docs")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .when(self.standalone.is_none(), |pane| {
                pane.child(self.document_toolbar(window, cx, colors))
            })
            .when_some(self.doc_error.as_ref(), |el, error| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .bg(colors.soft)
                        .text_color(colors.warning)
                        .child(error.clone()),
                )
            })
            .when(self.doc_loading.is_some(), |el| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_color(colors.muted)
                        .child(t!("workspace.opening_doc")),
                )
            })
            .when_some(self.doc.clone(), |el, reader| {
                el.child(reader.cached(div().w_full().flex_1().min_w_0().min_h_0().style().clone()))
            })
            .when(
                self.doc.is_none() && self.doc_loading.is_none() && self.doc_error.is_none(),
                |el| {
                    el.child(
                        h_flex()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .text_color(colors.muted)
                            .child(if self.docs_tree_loading {
                                t!("workspace.opening_doc")
                            } else if self.docs_tree.initial.is_some() {
                                t!("docs.select_from_tree")
                            } else {
                                t!("workspace.no_docs")
                            }),
                    )
                },
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.restore_file_viewport(window, cx);
        let scan = self.docs_scan_pending && self.view == WorkspaceView::Docs;
        let watch = self.repository_watch_pending
            && (self.view != WorkspaceView::Docs
                || (self.docs_tree_seeded
                    && (self.doc_loading.is_none() || self.docs_tree_complete)));
        // Let the initial content paint before starting the remaining repository work.
        if scan || watch {
            self.docs_scan_pending &= !scan;
            self.repository_watch_pending &= !watch;
            let repo = self.repository.clone();
            cx.on_next_frame(window, move |this, window, cx| {
                if this
                    .repository
                    .as_ref()
                    .zip(repo.as_ref())
                    .is_some_and(|(current, repo)| Arc::ptr_eq(current, repo))
                {
                    if scan {
                        this.scan_docs_tree(window, cx);
                    }
                    if watch {
                        this.watch_repository(window, cx);
                    }
                }
            });
        }
        let colors = Palette::for_mode(cx.theme().mode);
        let picker_opacity = self.repository_ui.picker_opacity(window, cx);
        let locator_overlay = self.locator_overlay(window, cx);
        let switching = self.repository_overlay_open();
        v_flex()
            .id("workspace")
            .size_full()
            .min_h_0()
            .text_color(colors.ink)
            .text_size(px(13.5))
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::toggle_settings))
            .on_action(cx.listener(|this, _: &OpenLocator, window, cx| {
                if this.standalone.is_none() && !this.terminal_focused(window, cx) {
                    this.open_locator(window, cx);
                }
            }))
            .on_action(cx.listener(Self::choose_repository))
            .on_action(cx.listener(Self::close_repository_picker))
            .on_action(cx.listener(|this, _: &FindInDocument, window, cx| this.find(window, cx)))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::toggle_wrap))
            .on_action(cx.listener(Self::toggle_terminal_shortcut))
            .when(self.view != WorkspaceView::Editor && !switching, |view| {
                view.key_context("Reader")
            })
            .when(self.view == WorkspaceView::Editor && !switching, |view| {
                view.key_context("WorkspaceEditor")
                    .on_action(cx.listener(Self::find_references))
                    .on_action(cx.listener(Self::source_back))
                    .on_action(cx.listener(Self::source_forward))
                    .capture_any_mouse_down(cx.listener(Self::source_mouse_down))
                    .on_action(cx.listener(Self::format_document))
                    .on_action(cx.listener(Self::close_active_tab))
                    .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                        this.cycle_tabs(true, window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &PreviousTab, window, cx| {
                        this.cycle_tabs(false, window, cx);
                    }))
            })
            .child(if self.standalone.is_some() {
                self.standalone_topbar(window, cx, colors)
                    .into_any_element()
            } else {
                self.topbar(window, cx, colors).into_any_element()
            })
            .when(
                self.repository
                    .as_ref()
                    .and_then(|repo| repo.ssh.as_ref())
                    .is_some_and(|ssh| ssh.failure().is_some()),
                |view| {
                    view.child(
                        h_flex()
                            .px_3()
                            .py_1()
                            .gap_3()
                            .bg(colors.soft)
                            .text_color(colors.warning)
                            .child(t!("workspace.ssh_disconnected"))
                            .child(
                                quiet_button("reconnect-ssh-banner")
                                    .label(t!("workspace.reconnect"))
                                    .disabled(self.repository_ui.opening.is_some())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reconnect_ssh(window, cx)
                                    })),
                            ),
                    )
                },
            )
            .child(if self.repository.is_none() && self.standalone.is_none() {
                self.file_drop_area(self.repository_start(window, cx, colors), cx, colors)
                    .into_any_element()
            } else if self.view == WorkspaceView::Git {
                self.file_drop_area(
                    div()
                        .id("git-content")
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .children(self.git.clone()),
                    cx,
                    colors,
                )
                .into_any_element()
            } else {
                self.workspace_body(window, cx, colors).into_any_element()
            })
            .when_some(locator_overlay, |view, overlay| view.child(overlay))
            .when(
                self.repository.is_some()
                    && (self.repository_ui.picker_open || picker_opacity > 0.),
                |view| view.child(self.repository_popup(window, cx, colors, picker_opacity)),
            )
            .when_some(self.image_preview_layer(cx), |view, layer| {
                view.child(layer)
            })
    }
}

fn nav_item(
    id: &'static str,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    active: bool,
    window: &Window,
    cx: &App,
    colors: Palette,
) -> Button {
    let label = label.into();
    let description = description.into();
    let label_size = px(text_base_size(cx));
    let description_size = px(11.);
    let mut ui_font = font(cx.theme().font_family.clone());
    ui_font.fallbacks = cx.theme().font_fallbacks.clone();
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&ui_font);
    let cap_height = text_system.cap_height(font_id, label_size);
    // Center the visible capitals around the row midpoint on a shared baseline.
    let baseline_offset = if cap_height > px(0.) {
        cap_height / 2.
    } else {
        text_system.ascent(font_id, label_size) / 2.
    };
    let text_run = |len, weight| TextRun {
        font: Font {
            weight,
            ..ui_font.clone()
        },
        ..TextStyle::default().to_run(len)
    };
    let regular = text_run(label.len(), FontWeight::NORMAL);
    let semibold = text_run(label.len(), FontWeight::SEMIBOLD);
    let subtitle = text_run(description.len(), FontWeight::NORMAL);
    let text_width = |value: &str, size, run: &TextRun| {
        text_system
            .layout_line(value, size, std::slice::from_ref(run), None)
            .width
            .ceil()
    };
    let label_width =
        text_width(&label, label_size, &regular).max(text_width(&label, label_size, &semibold));
    let description_width = text_width(&description, description_size, &subtitle);
    let text = |value: SharedString, size, run, width, color: Option<Hsla>| {
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                let run = TextRun {
                    color: color.unwrap_or_else(|| window.text_style().color),
                    ..run
                };
                let line = window.text_system().shape_line(value, size, &[run], None);
                // Both canvases fill the button's content row. Snap its shared
                // baseline here, after layout has snapped the row itself.
                let baseline = window.pixel_snap(bounds.center().y + baseline_offset);
                let origin = point(bounds.left(), baseline - line.ascent);
                if let Err(error) = line.paint(
                    origin,
                    line.ascent + line.descent,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                ) {
                    eprintln!("{error:#}");
                }
            },
        )
        .w(width)
        .h_full()
        .flex_shrink_0()
    };

    quiet_button(id)
        .with_size(px(20.))
        .rounded(px(6.))
        .h(px(24.))
        .px_3()
        .accessibility_label(label.clone())
        .selected(active)
        .when(active, |button| {
            button
                .bg(colors.elevated)
                .text_color(colors.accent)
                .shadow_xs()
        })
        .child(text(
            label,
            label_size,
            if active { semibold } else { regular },
            label_width,
            None,
        ))
        .child(text(
            description,
            description_size,
            subtitle,
            description_width,
            Some(colors.muted),
        ))
}

/// The vertical distance from a text baseline to the center of its line box is
/// `(ascent - descent) / 2`. A row using `items_center` therefore puts text of
/// different sizes on different baselines. Shift the smaller run by the
/// difference so it shares the reference run's baseline without changing its
/// font size.
pub(crate) fn baseline_shift(cx: &App, size: f32, reference: f32) -> Pixels {
    let text_system = cx.text_system();
    let font_id = text_system.resolve_font(&font(cx.theme().font_family.clone()));
    let half_offset = |size: f32| {
        (text_system.ascent(font_id, px(size)) - text_system.descent(font_id, px(size))) / 2.
    };
    half_offset(reference) - half_offset(size)
}

/// Wrap an element whose text baseline should follow `reference` while keeping
/// `size` unchanged. The element must be vertically centered in its row.
pub(crate) fn baseline_aligned(
    cx: &App,
    size: f32,
    reference: f32,
    element: impl IntoElement,
) -> Div {
    div()
        .relative()
        .top(baseline_shift(cx, size, reference))
        .child(element)
}

pub(crate) fn text_base_size(cx: &App) -> f32 {
    cx.theme().font_size.as_f32()
}

/// Font size for file and directory names in the document, workspace and Git
/// file trees, and for the file title/path in the Git diff header. Keep it just
/// below the UI text size so dense trees stay readable.
pub(crate) fn file_name_font_size(cx: &App) -> f32 {
    text_base_size(cx) - 0.5
}

pub(crate) fn icon(name: IconName, color: Hsla, size: f32) -> Icon {
    Icon::default()
        .path(name.path())
        .size(px(size))
        .text_color(color)
        .flex_shrink_0()
}

pub(crate) fn action_button(id: impl Into<ElementId>) -> Button {
    Button::new(id).rounded(px(5.))
}

fn quiet_button(id: impl Into<ElementId>) -> Button {
    action_button(id).ghost()
}

fn titlebar_actions() -> Div {
    // Keep the title bar from handling button presses as window drags.
    h_flex().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

pub(crate) fn icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
) -> Button {
    let label = label.into();
    quiet_button(id)
        .with_size(px(20.))
        .size(px(28.))
        .p_0()
        .icon(icon.into())
        .accessibility_label(label.clone())
        .tooltip(label)
}

pub(crate) fn compact_icon_button(
    id: impl Into<ElementId>,
    name: IconName,
    label: impl Into<SharedString>,
) -> Button {
    icon_button(id, name, label)
        .with_size(px(16.))
        .size(px(24.))
}

fn status_button(id: &'static str) -> Button {
    quiet_button(id)
        .with_size(px(16.))
        .h(px(22.))
        .px(px(5.))
        .content_gap(px(4.))
        .label_size(px(11.))
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}

fn language_for_source(path: &Path, source: &Rope) -> &'static str {
    if docs::is_markdown(path) {
        return "markdown";
    }
    let first_line: String = source
        .chars()
        .take(256)
        .take_while(|&ch| ch != '\n')
        .collect();
    if gpui_component::highlighter::is_bash_source(
        file_name(path),
        Some(first_line.trim_start_matches('\u{feff}')),
    ) {
        "bash"
    } else {
        language_for(path)
    }
}

fn language_for(path: &Path) -> &'static str {
    if docs::is_markdown(path) {
        return "markdown";
    }
    if gpui_component::highlighter::is_bash_source(file_name(path), None) {
        return "bash";
    }
    if file_name(path).eq_ignore_ascii_case("CMakeLists.txt") {
        return "cmake";
    }
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("cmake") => "cmake",
        Some("rs") => "rust",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "tsx",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("json") => "json",
        Some("jsonc") => "jsonc",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("py" | "pyi") => "python",
        Some("c" | "m") => "c",
        Some(
            "C" | "cc" | "cpp" | "cxx" | "c++" | "h" | "H" | "hh" | "hpp" | "hxx" | "h++" | "mm",
        ) => "cpp",
        Some("css") => "css",
        Some("html") => "html",
        _ => "text",
    }
}

fn preview_click(event: &ClickEvent) -> bool {
    !matches!(event, ClickEvent::Touch(_)) && event.click_count() < 2
}

#[cfg(test)]
mod tests {
    use super::{language_for, language_for_source};
    use gpui_component::{
        Rope,
        highlighter::{HighlightTheme, SyntaxHighlighter},
    };
    use std::path::Path;

    #[gpui::test]
    fn file_tree_edge_paints_inside_the_resize_band(cx: &mut gpui::TestAppContext) {
        use super::*;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        let repository = Arc::new(Repository::open(&root).unwrap());
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(move |window, cx| {
            let mut workspace =
                Workspace::with_config(store, config, WorkspaceView::Editor, window, cx);
            workspace.repository = Some(repository);
            workspace
        });
        visual.simulate_resize(size(px(800.), px(600.)));

        let black = Hsla::black();
        let gradient = linear_gradient(
            90.,
            linear_color_stop(black.opacity(0.), 0.),
            linear_color_stop(black.opacity(0.09), 1.),
        );
        let shadow = visual.update(|window, cx| {
            apply_theme(ThemeMode::Light, cx);
            window.draw(cx).clear(cx);
            window
                .painted_quads()
                .into_iter()
                .find(|quad| quad.background == gradient)
                .expect("light theme paints the file tree shadow")
                .bounds
        });
        let (drag_start, divider_x) = visual.update(|window, cx| {
            let scale = window.scale_factor();
            assert_eq!(px(shadow.size.width.0 / scale), px(4.));
            apply_theme(ThemeMode::Dark, cx);
            cx.set_reduce_motion(true);
            window.draw(cx).clear(cx);
            let quads = window.painted_quads();
            assert!(quads.iter().all(|quad| quad.background != gradient));
            let line = quads
                .iter()
                .find(|quad| quad.background == Background::from(rgb(0x3d4048)))
                .expect("dark theme paints a brighter divider")
                .bounds;
            assert_eq!(px(line.size.width.0 / scale), px(1.));
            assert_eq!(shadow.right(), line.left());
            assert_eq!(shadow.top(), line.top());
            assert_eq!(shadow.bottom(), line.bottom());
            (
                point(
                    px(line.right().0 / scale - 3.),
                    px(line.center().y.0 / scale),
                ),
                line.center().x,
            )
        });
        let draw_thumb = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                let scale = window.scale_factor();
                let accent = Palette::for_mode(ThemeMode::Dark).accent;
                window.painted_quads().iter().find_map(|quad| {
                    let color = quad.background.as_solid()?;
                    (quad.bounds.size.width.0 == 4. * scale
                        && quad.bounds.size.height.0 == 32. * scale
                        && color == accent.opacity(color.a))
                    .then_some(quad.bounds)
                })
            })
        };
        // Hovering across the seam does not engage the handle.
        visual.simulate_mouse_move(
            point(drag_start.x + px(5.), drag_start.y),
            None,
            Modifiers::default(),
        );
        visual.executor().advance_clock(Duration::from_millis(121));
        assert!(draw_thumb(visual).is_none());
        visual.simulate_mouse_move(drag_start, None, Modifiers::default());
        assert!(draw_thumb(visual).is_none());
        visual.executor().advance_clock(Duration::from_millis(121));
        let thumb = draw_thumb(visual).expect("hovering inside the four-pixel band");
        assert!((thumb.center().x.0 - divider_x.0).abs() <= 1.);
        let initial_width = visual.update(|_, cx| workspace.read(cx).editor_tree_width);
        visual.simulate_mouse_down(drag_start, MouseButton::Left, Modifiers::default());
        for offset in [10., 40.] {
            visual.simulate_mouse_move(
                point(drag_start.x + px(offset), drag_start.y),
                Some(MouseButton::Left),
                Modifiers::default(),
            );
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        visual.simulate_mouse_up(
            point(drag_start.x + px(40.), drag_start.y),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert_eq!(
            visual.update(|_, cx| workspace.read(cx).editor_tree_width),
            initial_width + px(40.),
        );
    }

    #[gpui::test]
    fn git_side_pane_shadows_stay_outside_the_diff(cx: &mut gpui::TestAppContext) {
        use super::*;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.join("changed.txt"), "Added line\n").unwrap();
        let repository = Arc::new(Repository::open(&root).unwrap());
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        cx.update(gpui_component::init);
        let (_, visual) = cx.add_window_view(move |window, cx| {
            let mut workspace =
                Workspace::with_config(store, config, WorkspaceView::Git, window, cx);
            workspace.repository = Some(repository);
            workspace.open_git_panel(window, cx);
            workspace
        });
        visual.simulate_resize(size(px(1280.), px(800.)));
        visual.run_until_parked();

        let gradient = |start, end| {
            let black = Hsla::black();
            linear_gradient(
                90.,
                linear_color_stop(black.opacity(start), 0.),
                linear_color_stop(black.opacity(end), 1.),
            )
        };
        let shadows = visual.update(|window, cx| {
            apply_theme(ThemeMode::Light, cx);
            window.draw(cx).clear(cx);
            let quads = window.painted_quads();
            [gradient(0., 0.09), gradient(0.09, 0.)].map(|gradient| {
                quads
                    .iter()
                    .find(|quad| quad.background == gradient)
                    .expect("each visible side pane paints an inward shadow")
                    .bounds
            })
        });
        let divider = Background::from(rgb(0x3d4048));
        visual.update(|window, cx| {
            apply_theme(ThemeMode::Dark, cx);
            window.draw(cx).clear(cx);
            let quads = window.painted_quads();
            assert!(quads.iter().all(|quad| {
                quad.background != gradient(0., 0.09) && quad.background != gradient(0.09, 0.)
            }));
            let mut lines: Vec<_> = quads
                .iter()
                .filter(|quad| quad.background == divider)
                .map(|quad| quad.bounds)
                .collect();
            lines.sort_by(|a, b| a.left().0.total_cmp(&b.left().0));
            let [history_line, tree_line] = lines.as_slice() else {
                panic!("dark theme paints two dividers");
            };
            for (shadow, line) in shadows.iter().zip(&lines) {
                let scale = window.scale_factor();
                assert_eq!(shadow.size.width.0 / scale, 4.);
                assert_eq!(line.size.width.0 / scale, 1.);
                assert_eq!(shadow.top(), line.top());
                assert_eq!(shadow.bottom(), line.bottom());
            }
            assert_eq!(shadows[0].right(), history_line.left());
            assert_eq!(shadows[1].left(), tree_line.right());
            assert!(history_line.right() < tree_line.left());
        });

        visual.simulate_resize(size(px(900.), px(800.)));
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert_eq!(
                window
                    .painted_quads()
                    .iter()
                    .filter(|quad| quad.background == divider)
                    .count(),
                1,
            );
        });
    }

    #[gpui::test]
    fn appearance_fonts_follow_theme_changes_and_reset(cx: &mut gpui::TestAppContext) {
        use super::{Theme, ThemeMode, apply_theme, apply_theme_with_fonts, ui_code_font};
        use crate::config::{CodeContrast, CodeTheme};
        use gpui::FontFallbacks;

        cx.update(|cx| {
            gpui_component::init(cx);
            apply_theme(ThemeMode::Light, cx);
            let default_ui = Theme::global(cx).font_family.clone();
            let default_mono = Theme::global(cx).mono_font_family.clone();
            let fallbacks = FontFallbacks::from_fonts(vec!["Configured fallback".into()]);

            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                apply_theme_with_fonts(
                    mode,
                    gpui::px(13.),
                    Some("Configured UI".into()),
                    Some("Configured code".into()),
                    Some(fallbacks.clone()),
                    CodeTheme::default(),
                    CodeContrast::Enhanced,
                    cx,
                );
                let theme = Theme::global(cx);
                assert_eq!(theme.font_family, "Configured UI");
                assert_eq!(theme.font_fallbacks.as_ref(), Some(&fallbacks));
                assert_eq!(theme.mono_font_family, default_mono);
                assert_eq!(ui_code_font(cx), "Configured code");
            }

            apply_theme(ThemeMode::Light, cx);
            let theme = Theme::global(cx);
            assert_eq!(theme.font_family, default_ui);
            assert!(theme.font_fallbacks.is_none());
            assert_eq!(ui_code_font(cx), default_mono);
        });
    }

    /// Transient messages live in the corner that never covers the buffer.
    #[gpui::test]
    fn toasts_stack_in_the_bottom_right(cx: &mut gpui::TestAppContext) {
        use super::{Theme, ThemeMode, apply_theme};

        cx.update(|cx| {
            gpui_component::init(cx);
            apply_theme(ThemeMode::Light, cx);
            assert_eq!(
                Theme::global(cx).notification.placement,
                super::toast::PLACEMENT
            );
        });
    }

    #[gpui::test]
    fn code_themes_follow_the_appearance_and_reset(cx: &mut gpui::TestAppContext) {
        use super::{Theme, ThemeMode, apply_theme, apply_theme_with_fonts};
        use crate::config::{CodeContrast, CodeTheme};

        cx.update(|cx| {
            gpui_component::init(cx);
            apply_theme_with_fonts(
                ThemeMode::Light,
                gpui::px(13.),
                None,
                None,
                None,
                CodeTheme::Paper,
                CodeContrast::High,
                cx,
            );
            let theme = Theme::global(cx);
            assert_eq!(theme.highlight_theme.name, "Paper Light");
            assert_eq!(
                theme.highlight_theme.style.editor_background,
                Some(gpui::rgb(0xfbf7ef).into())
            );
            assert_eq!(
                super::code_theme::surface(CodeTheme::Paper, ThemeMode::Light),
                gpui::rgb(0xfbf7ef).into()
            );
            // The code palette is contrast-corrected against the app surfaces.
            assert!(theme.highlight_theme.style("keyword").is_some());
            assert!(theme.highlight_theme.style("comment").is_some());

            apply_theme(ThemeMode::Light, cx);
            let theme = Theme::global(cx);
            assert_eq!(theme.highlight_theme.name, "Prism Light");
            assert_eq!(
                theme.highlight_theme.style.editor_background,
                Some(gpui::rgb(0xffffff).into())
            );
        });
    }

    #[test]
    fn detects_cmake_source_files() {
        for filename in [
            "build/CMakeLists.txt",
            "cmake/Compiler.cmake",
            "Compiler.CMAKE",
        ] {
            assert_eq!(
                language_for_source(
                    Path::new(filename),
                    &Rope::from("message(STATUS \"Hello\")")
                ),
                "cmake"
            );
        }
        assert_eq!(language_for(Path::new("CMakeLists.txt.bak")), "text");
    }

    #[test]
    fn bash_files_detect_language_and_use_tree_sitter() {
        let source = "#!/usr/bin/env -S bash -eu\nfor name in world; do echo \"$name\"; done\n";
        let rope = Rope::from(source);
        let theme = HighlightTheme::default_dark();

        for filename in [
            "run.sh",
            "run.bash",
            ".bashrc",
            ".bash_profile",
            ".bash_login",
            ".bash_logout",
            ".bash_aliases",
            "run",
        ] {
            let language = language_for_source(Path::new(filename), &rope);
            assert_eq!(language, "bash", "{filename}");
            if filename != "run" {
                assert_eq!(language_for(Path::new(filename)), "bash", "{filename}");
            }
            let mut highlighter = SyntaxHighlighter::new(language);
            assert!(highlighter.update(None, &rope, None));
            let styles = highlighter.styles(&(0..source.len()), &*theme);
            let keyword = source.find("for").unwrap();
            assert!(styles.iter().any(|(range, style)| {
                range.start <= keyword
                    && range.end >= keyword + 3
                    && Some(*style) == theme.style("keyword")
            }));
        }
        assert_eq!(
            language_for_source(Path::new("run"), &Rope::from("#!/bin/zsh")),
            "text"
        );
        assert_eq!(language_for_source(Path::new("run.txt"), &rope), "text");
    }

    #[test]
    fn c_and_cpp_files_have_syntax_highlighting() {
        let c = "#include <stdio.h>\n// Comment\nint main(void) { puts(\"hello\"); return 0; }";
        let cpp = "#include <cstdio>\n// Comment\nnamespace demo { int run() { std::puts(\"hello\"); return 0; } }";
        let theme = HighlightTheme::default_dark();

        for (filename, language, code) in [
            ("main.c", "c", c),
            ("main.cpp", "cpp", cpp),
            ("main.c++", "cpp", cpp),
            ("main.H", "cpp", cpp),
            ("main.h++", "cpp", cpp),
        ] {
            assert_eq!(language_for(Path::new(filename)), language);
            let mut highlighter = SyntaxHighlighter::new(language);
            assert!(highlighter.update(None, &Rope::from_str(code), None));
            let styles = highlighter.styles(&(0..code.len()), &*theme);
            let mut tokens = vec![
                ("#include", "keyword"),
                ("// Comment", "comment"),
                ("int", "type"),
                ("\"hello\"", "string"),
                ("return", "keyword"),
                ("0", "number"),
            ];
            if language == "cpp" {
                tokens.push(("namespace", "keyword"));
            }
            for (token, capture) in tokens {
                let start = code.find(token).unwrap();
                let end = start + token.len();
                let expected = theme.style(capture).unwrap();
                assert!(
                    styles.iter().any(|(range, style)| {
                        range.start <= start && range.end >= end && *style == expected
                    }),
                    "{filename}: {token:?} should be highlighted as {capture}",
                );
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod layout_tests {
    use super::*;
    use crate::config::ConfigFile;
    use crate::workspace::SaveFile;
    use gpui::{TestAppContext, TestDispatcher, VisualTestContext, px, size};
    use gpui_component::Root;
    use std::process::Command;

    #[::core::prelude::v1::test]
    fn definition_jump_reveals_wrapped_caret_on_open_and_at_eof() -> Result<()> {
        use gpui_component::input::RopeExt as _;
        use lsp_types::{Position, Range, ShowDocumentParams};

        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?.join("repo");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        let source = PathBuf::from("source.txt");
        let target = PathBuf::from("target.txt");
        let prefix = "data ".repeat(240) + "😀 ";
        let text = format!(
            "{}{prefix}definition\n{}last",
            "before\n".repeat(32),
            "after\n".repeat(48)
        );
        fs::write(root.join(&source), "definition\n")?;
        fs::write(root.join(&target), text)?;
        let uri: lsp_types::Uri = url::Url::from_file_path(root.join(&target))
            .unwrap()
            .as_str()
            .parse()?;
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
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(920.), px(510.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root,
                        file: Some(source.clone()),
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let workspace = workspace.read(cx);
            assert_eq!(workspace.tabs.active(), Some(source.as_path()));
            assert!(!workspace.files.contains_key(&target));
        });

        let jump = |visual: &mut VisualTestContext, line, character| {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    let start = Position::new(line, character);
                    this.open_definition(
                        ShowDocumentParams {
                            uri: uri.clone(),
                            external: None,
                            take_focus: Some(true),
                            selection: Some(Range::new(start, start)),
                        },
                        window,
                        cx,
                    );
                });
            });
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                let workspace = workspace.read(cx);
                assert_eq!(workspace.tabs.active(), Some(target.as_path()));
                let editor = workspace.files[&target].editor.read(cx);
                assert!(editor.focus_handle(cx).is_focused(window));
                let offset = editor.cursor();
                let caret = editor.range_to_bounds(&(offset..offset)).unwrap();
                let viewport = editor.input_bounds();
                assert!(caret.top() >= viewport.top() && caret.bottom() <= viewport.bottom());
                (
                    editor.text().offset_to_position(offset),
                    caret.top() - viewport.top(),
                    editor.line_height().unwrap(),
                    editor.projected_wrap_rows(line as usize),
                )
            })
        };
        let (position, inset, line_height, wrapped) =
            jump(visual, 32, prefix.encode_utf16().count() as u32);
        assert_eq!(position, Position::new(32, prefix.chars().count() as u32));
        assert!(wrapped > EDITOR_JUMP_CONTEXT_LINES);
        let expected = EDITOR_JUMP_CONTEXT_LINES * line_height;
        assert!(
            (inset - expected).abs() < px(1.),
            "Wrapped caret at {inset:?}, expected {expected:?}"
        );
        assert_eq!(jump(visual, 81, 4).0, Position::new(81, 4));
        Ok(())
    }

    /// Saving reports itself without moving the buffer.
    ///
    /// The workspace used to print the save notice in a strip between the tab
    /// bar and the editor, so every save pushed the text down and the editor
    /// jumped. Nothing a routine action reports may take part in this layout.
    #[::core::prelude::v1::test]
    fn saving_keeps_the_editor_in_place() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        let file = root.join("notes.txt");
        fs::write(&file, "first line\nsecond line\n")?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1150.), px(760.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root: root.clone(),
                        file: Some(PathBuf::from("notes.txt")),
                    },
                    window,
                    cx,
                );
            });
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        let editor_bounds = |visual: &mut VisualTestContext| {
            let tabs = visual
                .debug_bounds("workspace-tab-bar")
                .expect("tab bar bounds");
            let editor = visual
                .debug_bounds("workspace-editor")
                .expect("editor viewport bounds");
            assert_eq!(
                editor.top(),
                tabs.bottom(),
                "something is laid out between the tab bar and the editor"
            );
            editor
        };
        let before = editor_bounds(visual);
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let file = this.active_file().expect("open file");
                file.editor
                    .clone()
                    .update(cx, |editor, cx| editor.select_all(window, cx));
            });
        });
        visual.simulate_input("rewritten\n");
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        assert_eq!(fs::read_to_string(&file)?, "rewritten\n");
        let after = editor_bounds(visual);
        assert_eq!(before, after, "the editor moved after saving");
        Ok(())
    }

    /// A draft that conflicts with the file on disk asks before it saves.
    ///
    /// The question is a prompt, and the report is a toast; neither inserts a
    /// row between the tab bar and the editor.
    #[::core::prelude::v1::test]
    fn a_conflicted_draft_asks_before_it_saves() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        let file = root.join("notes.txt");
        fs::write(&file, "first line\n")?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1150.), px(760.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root: root.clone(),
                        file: Some(PathBuf::from("notes.txt")),
                    },
                    window,
                    cx,
                );
            });
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let file = this.files.get(Path::new("notes.txt")).expect("open file");
                file.editor
                    .clone()
                    .update(cx, |editor, cx| editor.select_all(window, cx));
            });
        });
        visual.simulate_input("draft text\n");
        fs::write(&file, "disk text\n")?;
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                // What the repository watcher does for a draft that changed on disk.
                this.files
                    .get_mut(Path::new("notes.txt"))
                    .expect("open file")
                    .document
                    .conflict = true;
                this.save(&SaveFile, window, cx);
            });
            assert!(
                window.has_active_dialog(cx),
                "saving a conflicted draft must ask"
            );
        });
        visual.run_until_parked();
        assert_eq!(
            fs::read_to_string(&file)?,
            "disk text\n",
            "the conflicted draft must not be written over the file"
        );
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.reload_from_disk(Path::new("notes.txt"), window, cx);
            });
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            let file = this.files.get(Path::new("notes.txt")).expect("open file");
            assert!(!file.document.conflict);
            assert_eq!(file.editor.read(cx).text().to_string(), "disk text\n");
        });
        Ok(())
    }

    /// The save itself finds a file that changed on disk and asks right away.
    ///
    /// Before the conflict flag is set, the first save writes through the
    /// checked baseline. Discovering the change there is the only chance to ask
    /// before the draft is lost, and the prompt is the whole message: no error
    /// toast next to it, and no toast for a state the tab badge already marks.
    #[::core::prelude::v1::test]
    fn a_save_that_finds_the_file_changed_asks_once() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        let file = root.join("notes.txt");
        fs::write(&file, "first line\n")?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1150.), px(760.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root: root.clone(),
                        file: Some(PathBuf::from("notes.txt")),
                    },
                    window,
                    cx,
                );
            });
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let file = this.files.get(Path::new("notes.txt")).expect("open file");
                file.editor
                    .clone()
                    .update(cx, |editor, cx| editor.select_all(window, cx));
            });
        });
        visual.simulate_input("draft text\n");
        fs::write(&file, "external text\n")?;
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        visual.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "the save that finds the change must ask"
            );
            assert_eq!(fs::read_to_string(&file).unwrap(), "external text\n");
        });
        assert!(
            visual.debug_bounds("notification-icon").is_none(),
            "the prompt is the only message for a conflict"
        );
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            let file = this.files.get(Path::new("notes.txt")).expect("open file");
            assert!(file.document.conflict);
        });
        Ok(())
    }
}
