use super::{PaneDrag, pane_edge, panel_resize};
use rust_i18n::t;

mod editor;
mod graph;
mod history_edit;
mod history_preview;
mod history_rows;
mod operations;
mod review;
mod revisions;
mod tree;

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme, Disableable as _, Selectable as _, Sizable as _, StyledExt,
    button::{Button, ButtonVariants as _},
    diff::{
        DiffEvent, DiffFile, DiffLinePosition, DiffMode, DiffSide, DiffState, DiffViewportAnchor,
    },
    h_flex,
    input::{EditorState, Input, InputEvent, InputState, TextareaState},
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    resizable::HandleEdge,
    scroll::{Scrollbar, ScrollbarMode},
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    tooltip::{ManagedTooltipExt as _, Tooltip},
    v_flex,
};
use gpui_kit_assets::IconName;

use crate::{
    config::{DiffLayout, Git as GitSettings},
    git::{self, Changes, Commit, Comparison, History, Mode, Side},
    repository::Repository,
    session::{
        DiffPosition, GitSelection as Selection, GitSession, HistoryPosition, WorktreeSession,
    },
};

use super::{
    Palette, Workspace, action_button, compact_icon_button, icon, icon_button, quiet_button,
    text_base_size,
};

const GIT_TEXT_SIZE: Pixels = px(12.);
const GIT_BADGE_SIZE: Pixels = px(11.);
const HISTORY_PAGE_THRESHOLD: usize = 8;

/// Line height for commit message text, as a multiple of its font size.
const MESSAGE_LINE_HEIGHT: f32 = 1.4;

fn pane_title_size(cx: &App) -> Pixels {
    px(text_base_size(cx) + 0.5)
}

fn diff_position(anchor: &DiffViewportAnchor) -> DiffPosition {
    DiffPosition {
        path: anchor.path().to_owned(),
        line: anchor
            .line()
            .map(|line| (line.side() == DiffSide::Original, line.line())),
        inset: anchor.offset().into(),
        horizontal: anchor.horizontal().x.into(),
    }
}

fn diff_anchor(position: &DiffPosition) -> DiffViewportAnchor {
    let path: SharedString = position.path.clone().into();
    let line = position.line.map(|(original, line)| {
        let side = if original {
            DiffSide::Original
        } else {
            DiffSide::Modified
        };
        DiffLinePosition::new(path.clone(), side, line)
    });
    DiffViewportAnchor::new(
        path,
        line,
        px(position.inset),
        point(px(position.horizontal), px(0.)),
    )
}

type Branches = SearchableVec<Choice>;

fn worktree_counts(status: &git::WorktreeStatus) -> (usize, usize, usize) {
    status.values().fold((0, 0, 0), |(s, u, n), file| {
        (
            s + usize::from(file.staged),
            u + usize::from(file.unstaged),
            n + usize::from(file.untracked),
        )
    })
}

#[derive(Clone)]
struct CommitRow {
    oid: SharedString,
    short: SharedString,
    subject: SharedString,
    author: SharedString,
    date: SharedString,
    committed_at: SharedString,
    body_preview: SharedString,
    merge: bool,
    first_parent: Option<SharedString>,
}

impl From<Commit> for CommitRow {
    fn from(commit: Commit) -> Self {
        Self {
            subject: commit.subject.into(),
            oid: commit.oid.into(),
            short: commit.short.into(),
            author: commit.author.into(),
            date: commit.committed_at.get(..10).unwrap_or("").into(),
            committed_at: commit.committed_at.into(),
            body_preview: history_preview::preview_body(commit.body),
            merge: commit.parents.len() > 1,
            first_parent: commit.parents.into_iter().next().map(Into::into),
        }
    }
}

#[derive(Clone)]
struct Choice {
    value: String,
    label: SharedString,
    remote: bool,
}

impl SelectItem for Choice {
    type Value = String;

    fn title(&self) -> SharedString {
        if self.value == "HEAD" {
            t!("git.detached_head").into()
        } else if self.remote {
            t!("git.remote_tracking", branch = self.label).into()
        } else {
            self.label.clone()
        }
    }

    fn value(&self) -> &String {
        &self.value
    }
}

#[derive(Clone)]
enum PatchState {
    Pending,
    Loading,
    Ready,
    Failed,
}

#[derive(Clone, Default)]
struct PatchDisplay {
    // Keep the published result while a replacement patch is being read.
    truncated: Option<bool>,
    content: Option<[u8; 32]>,
    error: Option<String>,
}

struct Review {
    detail: Arc<Changes>,
    patches: Vec<PatchState>,
    patch_display: Vec<PatchDisplay>,
    image_files: Vec<bool>,
    images: HashMap<usize, [review::DiffImage; 2]>,
    image_order: VecDeque<usize>,
    image_redraw_pending: bool,
    documents: Vec<DiffFile>,
    operations: HashMap<usize, review::FileOperations>,
    pending_assets: HashMap<usize, review::PatchAssets>,
    versions: HashMap<usize, git::FileVersion>,
    sources: HashMap<usize, editor::SnapshotEditors>,
    contexts: HashMap<usize, usize>,
    live: HashMap<usize, review::LivePatch>,
    by_path: Rc<HashMap<String, usize>>,
    collapsed: HashSet<String>,
    directories: Arc<BTreeSet<String>>,
    tree_rows: std::cell::RefCell<Option<tree::TreeRows>>,
    tree_motion: std::cell::RefCell<super::tree_motion::TreeMotion<tree::Row>>,
    summary_layout: Option<(review::CommitSummaryLayout, Pixels)>,
    openable: Vec<bool>,
    totals: (usize, usize),
    counts: Rc<HashMap<usize, (usize, usize)>>,
    stats: Rc<git::ChangeStats>,
    active: Option<usize>,
}

struct Snapshot {
    detail: Arc<Changes>,
    file: usize,
    layer: Option<git::Layer>,
    side: Side,
    editor: Option<Entity<EditorState>>,
    notice: Option<gpui::SharedString>,
    loading: bool,
}

#[derive(Clone, Copy)]
enum PaneKind {
    History,
    Review,
    Tree,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CompactPane {
    #[default]
    History,
    Tree,
    HistoryDismissed,
}

struct GitPane {
    panel: WeakEntity<GitPanel>,
    kind: PaneKind,
}

impl Render for GitPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.panel
            .update(cx, |panel, cx| {
                let colors = Palette::for_mode(cx.theme().mode);
                let layout = panel.layout(window.viewport_size().width);
                match self.kind {
                    PaneKind::History => panel
                        .history_panel(window, cx, colors, layout.history)
                        .into_any_element(),
                    PaneKind::Review => panel
                        .review_panel(layout, colors, window, cx)
                        .into_any_element(),
                    PaneKind::Tree => panel
                        .tree_panel(layout.tree, colors, window, cx)
                        .into_any_element(),
                }
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

struct PaneLayout {
    history: Pixels,
    tree: Pixels,
    center: Pixels,
    show_history: bool,
    show_tree: bool,
}

pub(super) struct GitPanel {
    root: PathBuf,
    repository: Arc<Repository>,
    backend: git::Backend,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    branches: Entity<SelectState<Branches>>,
    commit_input: Entity<InputState>,
    commit_message: Entity<TextareaState>,
    commit_amend: Option<operations::CommitAmend>,
    commit_state: Option<Result<git::CommitState, String>>,
    commit_state_task: Task<()>,
    ai_commit_pending: bool,
    ai_commit_task: Task<()>,
    filter_input: Entity<InputState>,
    diff: Entity<DiffState>,
    history: Option<History>,
    branch: String,
    commits: Rc<Vec<CommitRow>>,
    worktree_counts: (usize, usize, usize),
    graph: Rc<Vec<graph::GraphRow>>,
    graph_layout: graph::Graph,
    history_scroll: ListState,
    history_geometry: Option<(Pixels, Font, Pixels)>,
    history_focus: FocusHandle,
    history_pick: history_edit::HistoryPick,
    history_preparing: bool,
    history_progress: Option<git::HistoryProgress>,
    history_rewrite: Option<git::RewriteRecord>,
    startup_session: Option<GitSession>,
    restore_history: Option<HistoryPosition>,
    restore_pages: usize,
    history_pane: Entity<GitPane>,
    review_pane: Entity<GitPane>,
    tree_pane: Entity<GitPane>,
    tree_scroll: UniformListScrollHandle,
    tree_followed_file: Option<String>,
    worktree_directories: Arc<BTreeSet<String>>,
    query: String,
    filter: String,
    only_matching: bool,
    single_file: bool,
    selection: Option<Selection>,
    review: Option<Review>,
    snapshot: Option<Snapshot>,
    editor: editor::ReviewEditor,
    pending_file_reveal: Option<String>,
    pending_hunk_navigation: Option<review::HunkNavigation>,
    pending_editor_open: Option<String>,
    pending_position: Option<DiffLinePosition>,
    pending_review_anchor: Option<DiffViewportAnchor>,
    loading_session_diff: Option<DiffPosition>,
    patch_requests: VecDeque<usize>,
    viewport_queue: VecDeque<usize>,
    image_queue: VecDeque<usize>,
    prefetch_first: Option<usize>,
    prefetch_forward: bool,
    history_error: Option<String>,
    history_page_error: Option<String>,
    detail_error: Option<String>,
    stage_error: Option<String>,
    more: bool,
    history_loading: bool,
    commits_loading: bool,
    #[cfg(test)]
    commits_requests: usize,
    detail_loading: bool,
    staging: bool,
    confirm_restore: Option<(String, usize)>,
    whitespace: bool,
    split: bool,
    wrap: bool,
    show_history: bool,
    show_tree: bool,
    compact_pane: CompactPane,
    history_width: Pixels,
    tree_width: Pixels,
    show_details: bool,
    show_hunks: bool,
    history_task: Task<()>,
    repo_changes: git::watch::RepoChanges,
    commits_task: Task<()>,
    detail_task: Task<()>,
    patch_tasks: [Task<()>; 2],
    image_task: Option<(usize, Task<()>)>,
    assets_task: Task<()>,
    assets_loading: bool,
    metadata_tasks: [Task<()>; 2],
    live_tasks: HashMap<PathBuf, Task<()>>,
    patch_loading: [bool; 2],
    viewport_reads: Vec<usize>,
    stats_loading: bool,
    #[cfg(test)]
    review_read_delay: Duration,
    snapshot_task: Task<()>,
    stage_task: Task<()>,
    query_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl GitPanel {
    pub(super) fn new(
        repository: Arc<Repository>,
        workspace: WeakEntity<Workspace>,
        config: GitSettings,
        session: &WorktreeSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branches: Entity<SelectState<Branches>> =
            cx.new(|cx| SelectState::new(Vec::new().into(), None, window, cx).searchable(true));
        let query = session
            .git
            .as_ref()
            .map(|state| state.query.clone())
            .unwrap_or_default();
        let commit_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("git.commit_search"))
                .default_value(query.clone())
        });
        let filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("git.path_filter")));
        let commit_message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, usize::MAX)
                .placeholder(t!("git.commit_message"))
                .default_value(session.commit_draft.as_deref().unwrap_or_default())
        });
        let diff = cx.new(|cx| {
            DiffState::new([], cx)
                .with_expansion_lines(10)
                .with_syntax_max_line_length(0)
        });
        let editor = editor::ReviewEditor::new(window, cx);
        let panel = cx.weak_entity();
        let history_pane = cx.new(|_| GitPane {
            panel: panel.clone(),
            kind: PaneKind::History,
        });
        let review_pane = cx.new(|_| GitPane {
            panel: panel.clone(),
            kind: PaneKind::Review,
        });
        let tree_pane = cx.new(|_| GitPane {
            panel,
            kind: PaneKind::Tree,
        });
        let mut subscriptions = vec![
            cx.subscribe(&commit_message, |this: &mut Self, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    if this.ai_commit_pending {
                        this.cancel_ai_commit();
                    }
                    App::notify(cx, this.review_pane.entity_id());
                }
            }),
            editor.bind(cx, window, workspace.clone()),
            // Commands and data changes can affect more than one pane. Scroll
            // input dirties the pane that owns the viewport directly.
            cx.observe_self(|this, cx| {
                App::notify(cx, this.history_pane.entity_id());
                App::notify(cx, this.review_pane.entity_id());
                App::notify(cx, this.tree_pane.entity_id());
            }),
            cx.subscribe_in(&branches, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(branch)) = event {
                    this.select_branch(branch.clone(), window, cx);
                }
            }),
            cx.subscribe_in(&commit_input, window, |this, input, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().trim().to_owned();
                    this.query_task = cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(220))
                            .await;
                        _ = this.update_in(cx, move |this, window, cx| {
                            this.set_query(query, window, cx)
                        });
                    });
                } else if matches!(event, InputEvent::PressEnter { .. }) {
                    this.query_task = Task::ready(());
                    this.set_query(input.read(cx).value().trim().to_owned(), window, cx);
                }
            }),
            cx.subscribe(&filter_input, |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.filter = input.read(cx).value().trim().to_lowercase();
                    this.tree_scroll = UniformListScrollHandle::new();
                    this.tree_followed_file = None;
                    if this.only_matching {
                        this.sync_documents(cx);
                    }
                    cx.notify();
                }
            }),
            cx.subscribe_in(&diff, window, |this, _, event, window, cx| {
                if matches!(event, DiffEvent::Scrolled | DiffEvent::VisibleFilesChanged) {
                    this.follow_review_viewport(cx);
                }
                if matches!(event, DiffEvent::VisibleFilesChanged) {
                    this.prefetch_patches(window, cx);
                }
                if let DiffEvent::FileExpanded(path) | DiffEvent::FileCollapsed(path) = event
                    && let Some(review) = &mut this.review
                    && let Some(&ix) = review.by_path.get(path.as_str())
                {
                    this.pending_hunk_navigation = None;
                    if review.active != Some(ix) {
                        review.active = Some(ix);
                        App::notify(cx, this.tree_pane.entity_id());
                    }
                    let primary = review.detail.files[ix].path.clone();
                    let expanded = matches!(event, DiffEvent::FileExpanded(_));
                    if expanded {
                        review.collapsed.remove(&primary);
                    } else {
                        review.collapsed.insert(primary);
                    }
                    if expanded {
                        this.request_visible_patch(ix, window, cx);
                        this.prefetch_patches(window, cx);
                    }
                    cx.notify();
                }
            }),
            cx.observe(&diff, |this, _, cx| {
                App::notify(cx, this.review_pane.entity_id())
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active()
                    && this
                        .workspace
                        .read_with(cx, |workspace, _| {
                            workspace.view == super::WorkspaceView::Git
                                && !workspace.repository_watch_active
                        })
                        .unwrap_or(false)
                {
                    this.refresh_worktree(window, cx);
                }
            }),
        ];
        if let Some(owner) = workspace.upgrade() {
            subscriptions.push(cx.observe(&owner, |this, _, cx| {
                App::notify(cx, this.review_pane.entity_id());
                App::notify(cx, this.tree_pane.entity_id());
            }));
        }
        let history_scroll =
            ListState::new(0, ListAlignment::Top, px(0.)).with_width_independent_heights();
        history_scroll.set_pixel_aligned_scroll(true);
        let mut panel = Self {
            root: repository.root.clone(),
            backend: repository.git_backend(),
            repository,
            workspace,
            focus: cx.focus_handle(),
            branches,
            commit_input,
            commit_message,
            commit_amend: None,
            ai_commit_pending: false,
            ai_commit_task: Task::ready(()),
            commit_state: None,
            commit_state_task: Task::ready(()),
            filter_input,
            diff,
            history: None,
            branch: String::new(),
            commits: Rc::default(),
            worktree_counts: (0, 0, 0),
            graph: Rc::default(),
            graph_layout: graph::Graph::default(),
            history_scroll,
            history_geometry: None,
            history_focus: cx.focus_handle(),
            history_pick: history_edit::HistoryPick::default(),
            history_preparing: false,
            history_progress: None,
            history_rewrite: None,
            startup_session: session.git.clone(),
            restore_history: None,
            restore_pages: 0,
            history_pane,
            review_pane,
            tree_pane,
            tree_scroll: UniformListScrollHandle::new(),
            tree_followed_file: None,
            worktree_directories: session
                .git
                .as_ref()
                .map(|git| git.worktree_directories.clone())
                .unwrap_or_default(),
            query,
            filter: String::new(),
            only_matching: false,
            single_file: false,
            selection: None,
            review: None,
            snapshot: None,
            editor,
            pending_file_reveal: None,
            pending_hunk_navigation: None,
            pending_editor_open: None,
            pending_position: None,
            pending_review_anchor: None,
            loading_session_diff: None,
            patch_requests: VecDeque::new(),
            viewport_queue: VecDeque::new(),
            image_queue: VecDeque::new(),
            prefetch_first: None,
            prefetch_forward: true,
            history_error: None,
            history_page_error: None,
            detail_error: None,
            stage_error: None,
            more: false,
            history_loading: false,
            commits_loading: false,
            #[cfg(test)]
            commits_requests: 0,
            detail_loading: false,
            staging: false,
            confirm_restore: None,
            whitespace: config.ignore_whitespace,
            split: config.diff_layout == DiffLayout::Split,
            wrap: config.soft_wrap,
            show_history: session.panels.git_history,
            show_tree: session.panels.git_tree,
            compact_pane: CompactPane::default(),
            history_width: px(session.panels.git_history_width),
            tree_width: px(session.panels.git_tree_width),
            show_details: false,
            show_hunks: false,
            history_task: Task::ready(()),
            repo_changes: Default::default(),
            commits_task: Task::ready(()),
            detail_task: Task::ready(()),
            patch_tasks: std::array::from_fn(|_| Task::ready(())),
            image_task: None,
            assets_task: Task::ready(()),
            assets_loading: false,
            metadata_tasks: std::array::from_fn(|_| Task::ready(())),
            live_tasks: HashMap::new(),
            patch_loading: [false; 2],
            viewport_reads: Vec::new(),
            stats_loading: false,
            #[cfg(test)]
            review_read_delay: Duration::ZERO,
            snapshot_task: Task::ready(()),
            stage_task: Task::ready(()),
            query_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        panel.refresh(window, cx);
        panel
    }

    pub(super) fn apply_settings(
        &mut self,
        previous: GitSettings,
        config: GitSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if previous.diff_layout != config.diff_layout {
            self.split = config.diff_layout == DiffLayout::Split;
        }
        if previous.soft_wrap != config.soft_wrap && self.wrap != config.soft_wrap {
            self.toggle_wrap(window, cx);
        }
        if previous.ignore_whitespace != config.ignore_whitespace
            && self.whitespace != config.ignore_whitespace
        {
            self.toggle_whitespace(window, cx);
        }
        cx.notify();
    }

    pub(super) fn refresh_after_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh(window, cx);
    }

    pub(super) fn focus(&mut self, window: &mut Window, cx: &mut App) {
        self.pending_editor_open = None;
        if self
            .review
            .as_ref()
            .is_some_and(|review| !review.detail.files.is_empty())
            && self.snapshot.is_none()
        {
            self.editor.focus(window, cx);
        } else {
            self.focus.focus(window, cx);
        }
    }

    pub(super) fn refresh_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (input, key) in [
            (&self.commit_input, "git.commit_search"),
            (&self.filter_input, "git.path_filter"),
        ] {
            input.update(cx, |input, cx| {
                input.set_placeholder(crate::locale::text(key), window, cx);
            });
        }
        self.commit_message.update(cx, |input, cx| {
            input.set_placeholder(t!("git.commit_message"), window, cx);
        });
        self.refresh_placeholders(cx);
        cx.notify();
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_history(false, None, window, cx);
    }

    fn refresh_history(
        &mut self,
        only_changed: bool,
        check: Option<git::watch::RepoChanges>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history_loading = true;
        self.refresh_commit_state(window, cx);
        let retry_history = self.history_error.is_some();
        let backend = self.backend.clone();
        let verify_visible = only_changed && check.is_none();
        let check = check.unwrap_or_default();
        let verify_loaded = check.index || check.rescan;
        let mut paths = check.paths.into_iter().collect::<HashSet<_>>();
        if (verify_visible || check.rescan)
            && let Some(review) = &self.review
            && review::is_custom_worktree(&review.detail.comparison)
        {
            // Recheck loaded custom diffs that lack a verified file version.
            paths.extend(
                review
                    .detail
                    .files
                    .iter()
                    .zip(&review.patches)
                    .enumerate()
                    .filter(|(ix, (_, patch))| {
                        !matches!(patch, PatchState::Pending) && !review.versions.contains_key(ix)
                    })
                    .map(|(_, (file, _))| file.path.clone()),
            );
        }
        let versions = self
            .review
            .as_ref()
            .into_iter()
            .filter(|review| (verify_visible || verify_loaded) && review.detail.status.is_some())
            .flat_map(|review| {
                let visible = self
                    .diff
                    .read(cx)
                    .visible_paths()
                    .filter_map(|path| review.by_path.get(path.as_str()).copied())
                    .collect::<HashSet<_>>();
                review
                    .versions
                    .iter()
                    .filter(move |(ix, _)| verify_loaded || visible.contains(ix))
                    .map(|(&ix, version)| (review.detail.files[ix].path.clone(), version.clone()))
            })
            .collect::<Vec<_>>();
        let previous_tip = self
            .history
            .as_ref()
            .map(|history| (history.checkout.clone(), history.head.clone()));
        let previous_rewrite = self.history_rewrite.clone();
        self.history_task = cx.spawn_in(window, async move |this, cx| {
            let recover = this
                .read_with(cx, |this, cx| {
                    !this.writes_busy(cx)
                        && this
                            .workspace
                            .read_with(cx, |workspace, _| {
                                workspace
                                    .files
                                    .values()
                                    .all(|file| !file.is_dirty() && file.unconfirmed_save.is_none())
                            })
                            .unwrap_or(false)
                })
                .unwrap_or(false);
            let task = cx.background_spawn(async move {
                let recovery = recover.then(|| backend.recover_history_backup());
                let check_commit = recovery.is_some_and(|result| !matches!(result, Ok(false)));
                let history = backend.history()?;
                if !versions.is_empty() {
                    let files = versions
                        .iter()
                        .map(|(path, _)| path.clone())
                        .collect::<Vec<_>>();
                    let current = backend.file_versions(&files);
                    for ((path, previous), version) in versions.into_iter().zip(current) {
                        if version.as_ref() != Some(&previous) {
                            paths.insert(path);
                        }
                    }
                }
                let counts = worktree_counts(&history.changes);
                let progress = backend.history_progress();
                let rewrite = if previous_tip.as_ref().is_some_and(|(branch, oid)| {
                    branch == &history.checkout && oid == &history.head
                }) {
                    previous_rewrite
                } else {
                    backend.rewrite_record(history.checkout.as_deref(), history.head.as_deref())
                };
                Ok::<_, anyhow::Error>((history, counts, paths, progress, rewrite, check_commit))
            });
            let result = task.await;
            _ = this.update_in(cx, move |this, window, cx| {
                this.history_loading = false;
                match result {
                    Ok((history, counts, paths, progress, rewrite, check_commit)) => {
                        if check_commit {
                            this.refresh_commit_state(window, cx);
                        }
                        this.history_progress = progress;
                        this.history_rewrite = rewrite;
                        let checkout_changed = this.history.as_ref().is_some_and(|old| {
                            old.head != history.head || old.checkout != history.checkout
                        });
                        if checkout_changed {
                            this.clear_commit_amend(window, cx);
                        }
                        this.history_error = None;
                        if only_changed && !retry_history && this.history.as_ref() == Some(&history)
                        {
                            if !paths.is_empty() {
                                this.refresh_review_paths(paths, window, cx);
                            }
                            this.apply_repo_changes(window, cx);
                            cx.notify();
                            return;
                        }
                        let worktree_changed = this.history.as_ref().is_none_or(|old| {
                            old.changes != history.changes
                                || old.head != history.head
                                || old.checkout != history.checkout
                        });
                        this.worktree_counts = counts;
                        let first_load = this.history.is_none();
                        let previous_oid = this.branch_oid().map(str::to_owned);
                        let followed_checkout = this.history.as_ref().is_some_and(|history| {
                            this.branch == history.checkout.as_deref().unwrap_or("HEAD")
                        });
                        let checkout = history.checkout.as_deref().unwrap_or("HEAD");
                        if first_load
                            || followed_checkout
                            || !history
                                .branches
                                .iter()
                                .any(|branch| branch.reference == this.branch)
                        {
                            this.branch = checkout.to_owned();
                        }
                        let mut items = Vec::new();
                        if history.checkout.is_none() {
                            items.push(Choice {
                                value: "HEAD".into(),
                                label: SharedString::default(),
                                remote: false,
                            });
                        } else if !history
                            .branches
                            .iter()
                            .any(|branch| branch.reference == checkout)
                        {
                            items.push(Choice {
                                value: checkout.to_owned(),
                                label: branch_name(checkout).to_owned().into(),
                                remote: false,
                            });
                        }
                        items.extend(history.branches.iter().map(|branch| Choice {
                            value: branch.reference.clone(),
                            label: branch.label.clone().into(),
                            remote: branch.remote,
                        }));
                        let restore = this
                            .startup_session
                            .take()
                            .filter(|_| this.history_progress.is_none());
                        if let Some(state) = &restore
                            && items.iter().any(|item| item.value == state.reference)
                        {
                            this.branch.clone_from(&state.reference);
                            this.restore_history = state.history.clone();
                        }
                        let selected = this.branch.clone();
                        this.branches.update(cx, |state, cx| {
                            state.set_items(items.into(), window, cx);
                            state.set_selected_value(&selected, window, cx);
                        });
                        this.update_history_ref_heights(&history);
                        this.history = Some(history);
                        if checkout_changed
                            || !first_load && previous_oid.as_deref() != this.branch_oid()
                        {
                            this.clear_history_selection(window, cx);
                        }
                        if let Some(state) = restore
                            && let Some(selection) = state.selection
                        {
                            this.load_review_at(selection, false, state.diff, true, window, cx);
                        }
                        if this.selection.is_none()
                            || this.branch_oid().is_none()
                                && matches!(this.selection, Some(Selection::Commit(..)))
                        {
                            this.load_review(Selection::Worktree(Mode::All), false, window, cx);
                        }
                        if first_load
                            || retry_history && this.commits.is_empty()
                            || previous_oid.as_deref() != this.branch_oid()
                        {
                            this.load_commits(false, window, cx);
                        }
                        if this
                            .selection
                            .as_ref()
                            .is_some_and(Selection::includes_worktree)
                            && !this.detail_loading
                            && (!only_changed
                                || worktree_changed
                                || !paths.is_empty()
                                || previous_oid.as_deref() != this.branch_oid())
                        {
                            let mut paths = paths;
                            if !only_changed || previous_oid.as_deref() != this.branch_oid() {
                                paths.extend(this.review.as_ref().into_iter().flat_map(|review| {
                                    review.detail.files.iter().map(|file| file.path.clone())
                                }));
                            }
                            this.refresh_review_paths(paths, window, cx);
                        }
                    }
                    Err(error) => this.history_error = Some(error.to_string()),
                }
                this.apply_repo_changes(window, cx);
                cx.notify();
            });
        });
        if !only_changed {
            cx.notify();
        }
    }

    fn update_history_ref_heights(&self, history: &History) {
        let Some(previous) = &self.history else {
            return;
        };
        if previous.refs.len() == history.refs.len()
            && previous
                .refs
                .keys()
                .all(|oid| history.refs.contains_key(oid))
        {
            return;
        }

        let top = self.history_scroll.logical_scroll_top();
        let count = self.history_scroll.item_count();
        // Item zero is the worktree. Change only loaded rows whose decoration affects height.
        for (ix, (commit, graph)) in self.commits.iter().zip(self.graph.iter()).enumerate() {
            let item = ix + 1;
            let oid = commit.oid.as_str();
            let before = graph.height(previous.refs.contains_key(oid));
            let after = graph.height(history.refs.contains_key(oid));
            if item < count && before != after {
                self.history_scroll
                    .splice_with_height_hints(item..item + 1, [after]);
                if top.item_ix == item {
                    self.history_scroll.scroll_to(ListOffset {
                        offset_in_item: top.offset_in_item.min(after),
                        ..top
                    });
                }
            }
        }
    }

    pub(super) fn refresh_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.staging && !self.history_loading {
            self.refresh_history(true, None, window, cx);
        }
    }

    pub(super) fn repository_source_changed(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.invalidate_source_inlay_hints(&path, cx);
        self.schedule_snapshot_alignment(cx);
        self.editor.invalidate_source(path);
        App::notify(cx, self.review_pane.entity_id());
    }

    pub(super) fn repository_changed(
        &mut self,
        changes: git::watch::RepoChanges,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.repo_changes.merge(changes);
        self.apply_repo_changes(window, cx);
    }

    fn apply_repo_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.staging
            || self.history_loading
            || self.detail_loading
            || self.repo_changes.is_empty()
            || self.stats_loading
            || self.patch_loading[1]
            || self.has_visible_patch_read(cx)
        {
            return;
        }
        let changes = std::mem::take(&mut self.repo_changes);
        if !changes.refs
            && !changes.index
            && !changes.rescan
            && self
                .selection
                .as_ref()
                .is_some_and(Selection::includes_worktree)
            && self
                .review
                .as_ref()
                .is_some_and(|review| review.detail.status.is_some())
        {
            self.refresh_review_paths(changes.paths.into_iter().collect(), window, cx);
        } else {
            self.refresh_history(true, Some(changes), window, cx);
        }
    }

    fn has_visible_patch_read(&self, cx: &App) -> bool {
        if self.snapshot.is_some() || self.viewport_reads.is_empty() {
            return false;
        }
        self.review.as_ref().is_some_and(|review| {
            review
                .active
                .is_some_and(|ix| self.viewport_reads.contains(&ix))
                || self.diff.read(cx).visible_paths().any(|path| {
                    review
                        .by_path
                        .get(path.as_str())
                        .is_some_and(|ix| self.viewport_reads.contains(ix))
                })
        })
    }

    fn branch_oid(&self) -> Option<&str> {
        let history = self.history.as_ref()?;
        if self.branch == "HEAD" {
            history.head.as_deref()
        } else {
            history
                .branches
                .iter()
                .find(|branch| branch.reference == self.branch)
                .map(|branch| branch.oid.as_str())
        }
    }

    fn foreign_branch(&self) -> bool {
        self.history.as_ref().is_some_and(|history| {
            self.branch != history.checkout.as_deref().unwrap_or("HEAD")
                && self.branch_oid().is_some()
        })
    }

    fn select_branch(&mut self, branch: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.branch == branch {
            return;
        }
        let worktree = self
            .selection
            .as_ref()
            .is_some_and(Selection::includes_worktree);
        self.selection = None;
        self.history_pick.clear();
        self.branch = branch;
        self.restore_history = None;
        self.history_scroll.reset(0);
        self.commits = Rc::default();
        self.graph = Rc::default();
        self.load_commits(false, window, cx);
        if worktree || self.branch_oid().is_none() {
            self.load_review(Selection::Worktree(Mode::All), false, window, cx);
        } else if let Some(oid) = self.branch_oid().map(str::to_owned) {
            self.load_review(Selection::Commit(oid, None), false, window, cx);
        }
    }

    fn set_query(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.query == query {
            return;
        }
        self.query = query;
        self.clear_history_selection(window, cx);
        self.restore_history = None;
        self.history_scroll.reset(0);
        self.commits = Rc::default();
        self.graph = Rc::default();
        self.load_commits(false, window, cx);
    }

    fn clear_history_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.history_pick.clear();
        match self.selection.clone() {
            Some(selection @ Selection::CommitRange { .. }) => {
                self.load_review(selection.without_commit_range(), false, window, cx);
            }
            Some(selection @ Selection::Compare { .. }) => {
                self.selection = Some(selection.without_commit_range());
            }
            _ => {}
        }
    }

    fn load_commits(&mut self, more: bool, window: &mut Window, cx: &mut Context<Self>) {
        if more && (self.commits_loading || !self.more) {
            return;
        }
        self.commits_task = Task::ready(());
        self.commits_loading = false;
        self.history_error = None;
        if !more {
            self.history_page_error = None;
            self.more = false;
        }
        let Some(oid) = self.branch_oid().map(str::to_owned) else {
            self.commits = Rc::default();
            self.graph = Rc::default();
            self.restore_history = None;
            App::notify(cx, self.history_pane.entity_id());
            return;
        };
        self.commits_loading = true;
        #[cfg(test)]
        {
            self.commits_requests += 1;
        }
        let query = self.query.clone();
        let backend = self.backend.clone();
        let offset = if more { self.commits.len() } else { 0 };
        let mut layout = if more {
            self.graph_layout.clone()
        } else {
            graph::Graph::default()
        };
        let task = cx.background_spawn(async move {
            let page = backend.commits(&oid, &query, offset)?;
            let graph = layout.append(&page.commits, !query.is_empty());
            let commits = page
                .commits
                .into_iter()
                .map(CommitRow::from)
                .collect::<Vec<_>>();
            Ok::<_, anyhow::Error>((commits, page.more, graph, layout))
        });
        self.commits_task = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, move |this, window, cx| {
                this.commits_loading = false;
                match result {
                    Ok((commits, has_more, graph, layout)) => {
                        this.history_page_error = None;
                        this.graph_layout = layout;
                        if !more {
                            this.commits = Rc::default();
                            this.graph = Rc::default();
                        }
                        Rc::make_mut(&mut this.commits).extend(commits);
                        Rc::make_mut(&mut this.graph).extend(graph);
                        this.more = has_more;
                        this.continue_history_restore(window, cx);
                    }
                    Err(error) if more => this.history_page_error = Some(error.to_string()),
                    Err(error) => this.history_error = Some(error.to_string()),
                }
                App::notify(cx, this.history_pane.entity_id());
            });
        });
        App::notify(cx, self.history_pane.entity_id());
    }

    fn load_visible_history_page(
        &mut self,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.layout(window.viewport_size().width).show_history
            && self.history_page_error.is_none()
            && self.history_error.is_none()
            && self.commits.len() + 1 == count
            && self.history_scroll.item_count() == count
        {
            self.load_commits(true, window, cx);
        }
    }

    pub(super) fn commit_draft(&self, cx: &App) -> SharedString {
        self.commit_amend.as_ref().map_or_else(
            || self.commit_message.read(cx).value(),
            |amend| amend.draft.clone(),
        )
    }

    pub(super) fn capture_panel_preferences(&self, panels: &mut crate::session::PanelPreferences) {
        panels.git_history = self.show_history;
        panels.git_tree = self.show_tree;
        panels.git_history_width = self.history_width.into();
        panels.git_tree_width = self.tree_width.into();
    }

    fn set_history_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.show_history = visible;
        if visible {
            self.compact_pane = CompactPane::History;
        }
        cx.notify();
    }

    fn set_tree_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.show_tree = visible;
        if visible {
            self.compact_pane = CompactPane::Tree;
        }
        cx.notify();
    }

    pub(super) fn session(&self, cx: &App) -> Option<GitSession> {
        self.history.as_ref()?;
        let top = self.history_scroll.logical_scroll_top();
        let history = self.restore_history.clone().or_else(|| {
            let oid = if top.item_ix == 0 {
                None
            } else {
                Some(self.commits.get(top.item_ix - 1)?.oid.to_string())
            };
            Some(HistoryPosition {
                oid,
                inset: top.offset_in_item.into(),
            })
        });
        let selection = self.selection.clone();
        let diff = self
            .loading_session_diff
            .clone()
            .or_else(|| self.pending_review_anchor.as_ref().map(diff_position))
            .or_else(|| {
                self.diff
                    .read(cx)
                    .viewport_anchor()
                    .as_ref()
                    .map(diff_position)
            });
        Some(GitSession {
            reference: self.branch.clone(),
            query: self.query.clone(),
            diff: diff.filter(|_| !matches!(selection, Some(Selection::CommitRange { .. }))),
            selection: selection.map(Selection::without_commit_range),
            history,
            worktree_directories: self.worktree_directories.clone(),
        })
    }

    fn history_position(&self, saved: &HistoryPosition) -> Option<ListOffset> {
        let item_ix = match &saved.oid {
            Some(oid) => {
                self.commits
                    .iter()
                    .position(|row| row.oid.as_str() == oid)?
                    + 1
            }
            None => 0,
        };
        Some(ListOffset {
            item_ix,
            offset_in_item: px(saved.inset),
        })
    }

    fn continue_history_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const RESTORE_PAGE_LIMIT: usize = 30;
        if let Some(saved) = &self.restore_history
            && self.history_position(saved).is_none()
        {
            if self.more && self.restore_pages < RESTORE_PAGE_LIMIT {
                self.restore_pages += 1;
                self.load_commits(true, window, cx);
            } else {
                self.restore_history = None;
            }
        }
    }

    fn history_panel(
        &mut self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
        width: Pixels,
    ) -> impl IntoElement {
        let geometry = (width, window.text_style().font(), window.rem_size());
        if self.history_geometry.as_ref() != Some(&geometry) {
            // Keep estimated heights and the current item anchor while wrapped rows are remeasured.
            self.history_scroll.remeasure();
            self.history_geometry = Some(geometry);
        }
        let position = self
            .restore_history
            .as_ref()
            .and_then(|saved| self.history_position(saved));
        let history = self.history.as_ref();
        let commits = self.commits.clone();
        let graph = self.graph.clone();
        let refs = history
            .map(|history| history.refs.clone())
            .unwrap_or_default();
        let target = self
            .selection
            .as_ref()
            .map(|selection| selection.target_commit().map(str::to_owned));
        let picked = self.history_pick.rows.clone();
        let selected_count = picked.as_ref().map_or(0, |rows| rows.len());
        let comparison_target = target.as_ref().map(|target| {
            target.as_deref().map_or_else(
                || t!("git.current_worktree").into_owned(),
                |oid| git::short(oid).to_owned(),
            )
        });
        let weak = cx.weak_entity();
        let show_worktree = history.is_some();
        let count = commits.len() + usize::from(show_worktree);
        let page_available = self.more
            && !self.commits_loading
            && self.history_page_error.is_none()
            && self.history_error.is_none();
        let old_count = self.history_scroll.item_count();
        let old_count = if count < old_count {
            self.history_scroll.reset(0);
            0
        } else {
            old_count
        };
        if count > old_count {
            self.history_scroll.splice_with_height_hints(
                old_count..old_count,
                (old_count..count).map(|ix| {
                    if show_worktree && ix == 0 {
                        px(54.)
                    } else {
                        let ix = ix - usize::from(show_worktree);
                        graph[ix].height(refs.contains_key(commits[ix].oid.as_str()))
                    }
                }),
            );
        }
        if let Some(position) = position {
            self.history_scroll.scroll_to(position);
            self.restore_history = None;
        }
        let foreign = self.foreign_branch();
        let worktree_title: SharedString = if foreign {
            t!(
                "git.compare_to_worktree",
                branch = branch_name(&self.branch)
            )
            .into()
        } else {
            t!("git.uncommitted").into()
        };
        let worktree_count = history
            .filter(|_| !foreign)
            .map(|history| (history.changes.len(), self.worktree_counts));
        let base = if foreign {
            self.branch_oid()
        } else {
            history.and_then(|history| history.head.as_deref())
        };
        let connected = self.query.is_empty()
            && commits
                .first()
                .is_some_and(|commit| Some(commit.oid.as_str()) == base);
        let mode = cx.theme().mode;
        let main = graph::main_color(mode);
        let selected_color = colors.elevated.blend(main.opacity(0.09));
        let list = list(self.history_scroll.clone(), move |index, window, cx| {
            let title_y = Rc::new(std::cell::Cell::new(px(0.)));
            if show_worktree && index == 0 {
                let view = weak.clone();
                let selected = matches!(target, Some(None));
                return v_flex()
                    .id("git-worktree-row")
                    .relative()
                    .min_h(px(54.))
                    .w_full()
                    .justify_center()
                    .pl(history_rows::LEFT_INSET + graph::WORKTREE_WIDTH + history_rows::GRAPH_GAP)
                    .pr(history_rows::right_inset())
                    .py(px(5.))
                    .cursor_pointer()
                    .aria_selected(selected)
                    .when(selected, |row| row.bg(selected_color))
                    .when(!selected, |row| row.hover(|style| style.bg(colors.hover)))
                    .map(|row| {
                        let title = worktree_title.clone();
                        history_preview::attach(row, title_y.clone(), colors, move || {
                            history_preview::PreviewContent::worktree(title.clone(), worktree_count)
                        })
                    })
                    .on_click(move |_, window, cx| {
                        _ = view.update(cx, |this, cx| {
                            this.history_focus.focus(window, cx);
                            this.select_worktree(window, cx);
                        });
                    })
                    .child(
                        graph::worktree(connected, mode)
                            .absolute()
                            .left(history_rows::LEFT_INSET)
                            .top_0()
                            .bottom_0(),
                    )
                    .child(history_rows::worktree_content(
                        worktree_title.clone(),
                        worktree_count,
                        colors,
                        main,
                        cx,
                        title_y,
                    ))
                    .into_any_element();
            }
            let ix = index - usize::from(show_worktree);
            let commit = &commits[ix];
            let selected = picked.as_ref().map_or_else(
                || {
                    target.as_ref().and_then(|target| target.as_deref())
                        == Some(commit.oid.as_str())
                },
                |rows| rows.contains(&commit.oid),
            );
            let oid = commit.oid.clone();
            let view = weak.clone();
            let menu_view = weak.clone();
            let base = oid.clone();
            let subject = commit.subject.clone();
            let merge = commit.merge;
            let left = history_rows::LEFT_INSET + graph[ix].width() + history_rows::GRAPH_GAP;
            let content_width = (width - px(1.) - left - history_rows::right_inset()).max(px(0.));
            v_flex()
                .id(commit.oid.clone())
                .relative()
                .min_h(graph[ix].height(refs.contains_key(commit.oid.as_str())))
                .w_full()
                .justify_center()
                .pl(left)
                .pr(history_rows::right_inset())
                .py(px(5.))
                .cursor_pointer()
                .aria_selected(selected)
                .when(selected, |row| row.bg(selected_color))
                .when(!selected, |row| row.hover(|style| style.bg(colors.hover)))
                .map(|row| {
                    let commit = commit.clone();
                    let refs = refs.clone();
                    history_preview::attach(row, title_y.clone(), colors, move || {
                        history_preview::PreviewContent::commit(&commit, &refs)
                    })
                })
                .on_click(move |event: &ClickEvent, window, cx| {
                    _ = view.update(cx, |this, cx| {
                        this.history_focus.focus(window, cx);
                        this.pick_commit(&oid, event.modifiers(), true, window, cx);
                    });
                })
                .child(
                    graph::graph(graph.clone(), ix, mode)
                        .absolute()
                        .left(history_rows::LEFT_INSET)
                        .top_0()
                        .bottom_0(),
                )
                .child(history_rows::commit_content(
                    commit,
                    &refs,
                    content_width,
                    graph[ix].color(mode),
                    colors,
                    window,
                    cx,
                    title_y,
                ))
                .map(|row| {
                    let comparison_target = comparison_target.clone();
                    row.context_menu(move |menu, window, cx| {
                        let backend = menu_view
                            .read_with(cx, |panel, _| {
                                let selection = panel.cherry_pick_selection(&base)?;
                                Some((panel.backend.clone(), selection))
                            })
                            .ok()
                            .flatten();
                        let menu_view = menu_view.clone();
                        let comparison_target = comparison_target.clone();
                        let base = base.clone();
                        let subject = subject.clone();
                        let build = move |mut menu: PopupMenu,
                                          cherry_pick: Option<&(git::HistoryTip, Vec<String>)>,
                                          cx: &App| {
                            if let Some(target) = comparison_target.clone() {
                                let view = menu_view.clone();
                                let base = base.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(t!("git.compare_with", target = target))
                                        .icon(IconName::GitCompareArrows)
                                        .on_click(move |_, window, cx| {
                                            _ = view.update(cx, |this, cx| {
                                                this.compare_with_base(&base, window, cx)
                                            });
                                        }),
                                );
                            }
                            if let Ok(items) = menu_view.read_with(cx, |panel, cx| {
                                let mut items =
                                    panel.history_commands(&base, &subject, merge, &menu_view, cx);
                                if let Some(selection) = cherry_pick {
                                    items.extend(
                                        panel.cherry_pick_item(&base, selection, &menu_view, cx),
                                    );
                                }
                                items
                            }) && !items.is_empty()
                            {
                                menu = menu.separator();
                                for item in items {
                                    menu = menu.item(item);
                                }
                            }
                            if let Ok(item) =
                                menu_view.read_with(cx, |panel, _| panel.mark_commits_item(&base))
                            {
                                menu = menu.separator().item(item);
                            }
                            menu
                        };
                        let menu = build(menu, None, cx);
                        if let Some((backend, expected)) = backend {
                            let (tip, targets) = expected.clone();
                            let task = cx.background_spawn(async move {
                                backend.can_cherry_pick(&tip, &targets).unwrap_or(false)
                            });
                            cx.spawn_in(window, async move |menu, cx| {
                                if task.await {
                                    _ = menu.update_in(cx, |menu, window, cx| {
                                        menu.rebuild(window, cx, |menu, _, cx| {
                                            build(menu, Some(&expected), cx)
                                        });
                                    });
                                }
                            })
                            .detach();
                        }
                        menu
                    })
                    .into_any_element()
                })
        })
        .size_full();
        v_flex()
            .id("git-history")
            .relative()
            .w(width)
            .h_full()
            .flex_shrink_0()
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px_3()
                    .pb_2()
                    .gap_2()
                    .child(
                        h_flex()
                            .debug_selector(|| "git-history-toolbar".into())
                            .h(GIT_TOOLBAR_HEIGHT)
                            .flex_shrink_0()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .font_semibold()
                                    .text_size(pane_title_size(cx))
                                    .when(selected_count > 1, |title| {
                                        title.debug_selector(|| "git-history-selected-count".into())
                                    })
                                    .child(if selected_count > 1 {
                                        t!("git.history_edit_selected", count = selected_count)
                                    } else {
                                        t!("git.history")
                                    }),
                            )
                            .child(
                                icon_control(
                                    "git-refresh",
                                    IconName::RefreshCw,
                                    t!("git.refresh"),
                                    false,
                                )
                                .disabled(self.staging)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.refresh(window, cx)),
                                ),
                            )
                            .child(
                                icon_control(
                                    "git-hide-history",
                                    IconName::PanelLeftClose,
                                    t!("git.close_history"),
                                    false,
                                )
                                .debug_selector(|| "git-hide-history".into())
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.set_history_visible(false, cx);
                                    },
                                )),
                            ),
                    )
                    .child(
                        div()
                            .id("git-branch-picker")
                            .debug_selector(|| "git-branch-picker".into())
                            .w_full()
                            .when(history.is_some() && self.branch != "HEAD", |field| {
                                let name = SharedString::from(branch_name(&self.branch).to_owned());
                                field.managed_tooltip(move |window, cx| {
                                    Tooltip::new(name.clone()).build(window, cx)
                                })
                            })
                            .child(
                                Select::new(&self.branches)
                                    .w_full()
                                    .menu_width(px(290.))
                                    .icon(icon(IconName::GitBranch, colors.accent, 15.))
                                    .accessibility_label(t!("git.browse_branches"))
                                    .search_placeholder(t!("git.search_branches"))
                                    .placeholder(t!("git.select_branch"))
                                    .disabled(history.is_none()),
                            ),
                    )
                    .child(
                        div().debug_selector(|| "git-commit-search".into()).child(
                            Input::new(&self.commit_input)
                                .w_full()
                                .cleanable(true)
                                .prefix(icon(IconName::Search, colors.muted, 13.)),
                        ),
                    ),
            )
            .when_some(self.history_error.clone(), |panel, error| {
                panel.child(message(error, colors.warning))
            })
            .when(count > 0, |panel| {
                panel.child(
                    div()
                        .relative()
                        .track_focus(&self.history_focus)
                        .tab_stop(true)
                        .on_key_down(cx.listener(Self::history_key_down))
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .when(self.commits.is_empty(), |panel| {
                            panel.flex_none().h(px(54.))
                        })
                        .child(list)
                        .when(page_available, |viewport| {
                            let scroll = self.history_scroll.clone();
                            let panel = cx.weak_entity();
                            viewport
                                .child(
                                    canvas(
                                        move |_, window, cx| {
                                            let offset = scroll.logical_scroll_top();
                                            if (offset.item_ix != 0
                                                || offset.offset_in_item > px(0.))
                                                && count.saturating_sub(scroll.visible_range().end)
                                                    <= HISTORY_PAGE_THRESHOLD
                                            {
                                                let panel = panel.clone();
                                                window.defer(cx, move |window, cx| {
                                                    _ = panel.update(cx, |this, cx| {
                                                        this.load_visible_history_page(
                                                            count, window, cx,
                                                        );
                                                    });
                                                });
                                            }
                                        },
                                        |_, (), _, _| {},
                                    )
                                    .absolute()
                                    .size_0(),
                                )
                                .on_scroll(cx.listener(
                                    move |this, event: &ScrollEvent, window, cx| {
                                        // A tall viewport can fit the entire page before it can scroll.
                                        if event.delta.content_delta().y < px(0.)
                                            && this.history_scroll.max_offset_for_scrollbar().y
                                                == px(0.)
                                        {
                                            this.load_visible_history_page(count, window, cx);
                                        }
                                    },
                                ))
                        })
                        .child(
                            div().absolute().inset_0().child(
                                super::pane_scrollbar(&self.history_scroll)
                                    .id("git-history-scroll")
                                    .viewport_from_layout(),
                            ),
                        ),
                )
            })
            .when(
                self.commits.is_empty() && self.history_error.is_none(),
                |panel| {
                    panel.child(message(
                        if self.history.is_none() && self.history_loading || self.commits_loading {
                            t!("git.loading_commits")
                        } else if !self.query.is_empty() {
                            t!("git.no_matching_commits")
                        } else {
                            t!("git.empty_branch")
                        },
                        colors.muted,
                    ))
                },
            )
            .when(
                self.commits_loading && self.more && self.history_page_error.is_none(),
                |panel| {
                    panel.child(
                        div()
                            .absolute()
                            .bottom_0()
                            .left_2()
                            .right_4()
                            .bg(colors.elevated)
                            .child(message(t!("git.loading_commits"), colors.muted)),
                    )
                },
            )
            .when_some(self.history_page_error.clone(), |panel, error| {
                panel.child(
                    h_flex()
                        .gap_1()
                        .p_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(message(error, colors.warning)),
                        )
                        .child(
                            quiet_button("git-retry-history-page")
                                .small()
                                .label(t!("common.retry"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.load_commits(true, window, cx);
                                })),
                        ),
                )
            })
            .child(panel_resize(
                "git-resize-history",
                HandleEdge::Trailing,
                GIT_TOOLBAR_HEIGHT,
            ))
            .child(pane_edge(
                HandleEdge::Trailing,
                GIT_TOOLBAR_HEIGHT,
                cx.theme().mode,
            ))
    }

    fn select_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_history_target(None, window, cx);
    }

    fn select_history_target(
        &mut self,
        target: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history_pick.clear();
        let current = self.selection.as_ref().filter(|selection| {
            !matches!(selection, Selection::CommitRange { .. })
                && selection.target_commit() == target
        });
        let selection = match current {
            Some(selection) => selection.clone().without_commit_range(),
            None => target.map_or(Selection::Worktree(Mode::All), |oid| {
                Selection::Commit(oid.to_owned(), None)
            }),
        };
        self.load_review(selection, false, window, cx);
        if window.viewport_size().width < px(850.) {
            self.compact_pane = CompactPane::HistoryDismissed;
            cx.notify();
        }
    }
}

impl GitPanel {
    fn layout(&self, width: Pixels) -> PaneLayout {
        let history = self
            .history_width
            .clamp(px(230.), (width * 0.36).max(px(230.)));
        let tree = self
            .tree_width
            .clamp(px(215.), (width * 0.32).max(px(215.)));
        let has_tree = self.show_tree && self.review.is_some();
        let show_history = self.show_history
            && !(width < px(1000.) && self.compact_pane == CompactPane::Tree && has_tree)
            && !(width < px(850.) && self.compact_pane == CompactPane::HistoryDismissed);
        let show_tree = has_tree && (width >= px(1000.) || !show_history);
        let center = width
            - if show_history { history } else { px(0.) }
            - if show_tree { tree } else { px(0.) };
        PaneLayout {
            history,
            tree,
            center,
            show_history,
            show_tree,
        }
    }
}

impl Render for GitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = window.viewport_size().width;
        let layout = self.layout(width);
        if !layout.show_tree && self.tree_followed_file.take().is_some() {
            App::notify(cx, self.tree_pane.entity_id());
        }
        let mode = if self.split && layout.center >= px(580.) {
            DiffMode::Split
        } else {
            DiffMode::Unified
        };
        // A layout change must use the patch that includes pending local edits.
        if self.diff.read(cx).mode() != mode && !self.editor.has_pending_projection(self) {
            self.diff.update(cx, |diff, cx| diff.set_mode(mode, cx));
        }
        h_flex()
            .id("git-panel")
            .track_focus(&self.focus)
            .key_context("GitReview")
            .on_action(
                cx.listener(|this, _: &gpui_component::input::Undo, window, cx| {
                    this.replay_worktree_history(true, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &gpui_component::input::Redo, window, cx| {
                    this.replay_worktree_history(false, window, cx);
                }),
            )
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<PaneDrag>, _, cx| {
                    let pointer =
                        event.event.position.x - event.bounds.left() - event.drag(cx).offset();
                    match event.drag(cx).edge {
                        HandleEdge::Trailing => {
                            this.history_width =
                                pointer.clamp(px(230.), (width * 0.36).max(px(230.)))
                        }
                        HandleEdge::Leading => {
                            this.tree_width = (event.bounds.size.width - pointer)
                                .clamp(px(215.), (width * 0.32).max(px(215.)))
                        }
                    }
                    cx.notify();
                }),
            )
            .when(layout.show_history, |panel| {
                panel.child(
                    self.history_pane.clone().cached(
                        div()
                            .w(layout.history)
                            .h_full()
                            .flex_shrink_0()
                            .style()
                            .clone(),
                    ),
                )
            })
            .child(
                self.review_pane
                    .clone()
                    .cached(div().flex_1().h_full().min_w_0().min_h_0().style().clone()),
            )
            .when(layout.show_tree, |panel| {
                panel.child(
                    self.tree_pane.clone().cached(
                        div()
                            .w(layout.tree)
                            .h_full()
                            .flex_shrink_0()
                            .style()
                            .clone(),
                    ),
                )
            })
    }
}

fn subject(commit: &Commit) -> SharedString {
    if commit.subject.is_empty() {
        t!("git.empty_subject").into()
    } else {
        commit.subject.clone().into()
    }
}

fn branch_name(reference: &str) -> &str {
    reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/remotes/"))
        .unwrap_or(reference)
}

fn icon_control(
    id: &'static str,
    glyph: IconName,
    label: impl Into<SharedString>,
    selected: bool,
) -> Button {
    icon_button(id, glyph, label)
        .selected(selected)
        .toggled(selected)
}

fn file_count(count: usize) -> Cow<'static, str> {
    if count == 1 {
        t!("git.file_count_one", count = count)
    } else {
        t!("git.file_count_other", count = count)
    }
}

fn message(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .w_full()
        .px_3()
        .py_3()
        .text_size(GIT_TEXT_SIZE)
        .text_color(color)
        .child(text.into())
}

const GIT_TOOLBAR_HEIGHT: Pixels = super::FILE_TOOLBAR_HEIGHT;
// The resize handle occupies the inside edge of the pane.
const GIT_RESIZE_GUTTER: Pixels = super::PANE_RESIZE_GUTTER;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{fs, path::Path, process::Command};

    use super::*;
    use crate::repository::{Location, Store};
    use crate::workspace::WorkspaceView;

    mod ai_commit_tests;
    mod collapse_render_tests;
    #[cfg(target_os = "linux")]
    mod diff_scroll_benchmark;
    mod history_render_tests;
    mod inlay_hints_tests;
    mod navigation_render_tests;

    fn git(root: &Path, args: &[&str]) {
        let result = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn color_bytes(color: gpui::Hsla) -> [u8; 4] {
        let rgba: gpui::Rgba = color.into();
        [
            (rgba.r * 255.).round() as u8,
            (rgba.g * 255.).round() as u8,
            (rgba.b * 255.).round() as u8,
            (rgba.a * 255.).round() as u8,
        ]
    }

    #[::core::prelude::v1::test]
    fn git_tree_follows_the_review_and_respects_manual_browsing() {
        fn draw(visual: &mut VisualTestContext) {
            for _ in 0..5 {
                visual.run_until_parked();
                visual.update(|window, cx| {
                    window.simulate_next_frame(cx);
                    window.draw(cx).clear(cx);
                });
            }
        }

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir_all(root.join("folder")).unwrap();
        git(&root, &["init", "-qb", "main"]);
        git(&root, &["config", "user.name", "Reviewer"]);
        git(&root, &["config", "user.email", "reviewer@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        let paths = (0..36)
            .map(|ix| format!("folder/{ix:02}.txt"))
            .chain(["outside.txt".into()])
            .collect::<Vec<_>>();
        for path in &paths {
            fs::write(root.join(path), "original\n").unwrap();
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "initial"]);
        for path in &paths {
            fs::write(root.join(path), "changed\n").unwrap();
        }
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
        });
        visual.simulate_resize(size(px(1280.), px(470.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(Location { root, file: None }, window, cx);
            });
        });
        draw(visual);
        let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
        let active = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                let panel = panel.read(cx);
                panel
                    .active_file(cx)
                    .map(|ix| panel.review.as_ref().unwrap().detail.files[ix].path.clone())
            })
        };
        let tree = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| panel.read(cx).tree_scroll.0.borrow().base_handle.clone())
        };
        let row = |visual: &mut VisualTestContext, index: usize| {
            visual.update(|_, cx| {
                let state = panel.read(cx).tree_scroll.0.borrow();
                let viewport = state.base_handle.bounds();
                let height =
                    state.last_item_size.unwrap().contents.height / (paths.len() + 1) as f32;
                Bounds::new(
                    point(
                        viewport.left(),
                        viewport.top() + state.base_handle.offset().y + height * index as f32,
                    ),
                    size(viewport.size.width, height),
                )
            })
        };
        let show_tree = |visual: &mut VisualTestContext, show| {
            visual.update(|_, cx| {
                panel.update(cx, |panel, cx| {
                    panel.show_tree = show;
                    cx.notify();
                });
            });
            draw(visual);
        };
        let target = "folder/28.txt";
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.show_history = false;
                panel.split = false;
                panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
                cx.notify();
            });
        });
        draw(visual);
        assert_eq!(tree(visual).offset().y, px(0.));
        visual.update(|_, cx| {
            panel.read(cx).diff.clone().update(cx, |diff, cx| {
                diff.scroll_to_file(target, cx);
            });
        });
        crate::workspace::finish_scroll_navigation(visual);
        assert_eq!(active(visual).as_deref(), Some(target));
        assert!(tree(visual).offset().y < px(0.));
        let viewport = tree(visual).bounds();
        let target_row = row(visual, 29);
        assert!(
            target_row.top() >= viewport.top() - px(1.)
                && target_row.bottom() <= viewport.bottom() + px(1.),
            "target row {target_row:?} is outside the tree {viewport:?}"
        );

        let scroll_tree_to_top = |visual: &mut VisualTestContext| {
            let position = tree(visual).bounds().center();
            visual.simulate_event(ScrollEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.), px(1600.))),
                modifiers: Modifiers::default(),
                touch_phase: TouchPhase::Moved,
                source: ScrollSource::Pointer,
            });
            draw(visual);
            assert_eq!(tree(visual).offset().y, px(0.));
            assert_eq!(active(visual).as_deref(), Some(target));
        };
        scroll_tree_to_top(visual);
        show_tree(visual, false);
        show_tree(visual, true);
        assert!(tree(visual).offset().y < px(0.));
        scroll_tree_to_top(visual);

        let folder = row(visual, 0).center();
        visual.simulate_click(folder, Modifiers::default());
        draw(visual);
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            assert!(
                panel
                    .review
                    .as_ref()
                    .unwrap()
                    .directories
                    .contains("folder")
            );
            assert!(panel.tree_followed_file.is_none());
        });
        assert_eq!(active(visual).as_deref(), Some(target));
        visual.simulate_click(folder, Modifiers::default());
        draw(visual);
        assert!(tree(visual).offset().y < px(0.));

        let set_filter = |visual: &mut VisualTestContext, value| {
            visual.update(|window, cx| {
                panel.read(cx).filter_input.clone().update(cx, |input, cx| {
                    input.replace_all(value, window, cx);
                });
            });
            draw(visual);
        };
        set_filter(visual, "outside");
        assert_eq!(active(visual).as_deref(), Some(target));
        assert_eq!(tree(visual).offset().y, px(0.));
        visual.update(|_, cx| assert!(panel.read(cx).tree_followed_file.is_none()));
        set_filter(visual, "");
        assert!(tree(visual).offset().y < px(0.));

        scroll_tree_to_top(visual);
        let first_file = row(visual, 1).center();
        visual.simulate_click(first_file, Modifiers::default());
        draw(visual);
        assert_eq!(active(visual).as_deref(), Some("folder/00.txt"));
        visual.update(|_, cx| {
            assert_eq!(
                panel
                    .read(cx)
                    .diff
                    .read(cx)
                    .top_file()
                    .map(SharedString::as_str),
                Some("folder/00.txt")
            );
        });
    }

    // The staging slot extends past the indicator. Hover and clicks stay in
    // the staging control while the rest of the row keeps its own behavior.
    #[::core::prelude::v1::test]
    fn stage_slot_covers_the_cell_and_hands_clicks_to_staging() {
        use crate::config::ConfigFile;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir_all(root.join("folder")).unwrap();
        fs::write(root.join("folder/visible.txt"), "original\n").unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Reviewer"]);
        git(&root, &["config", "user.email", "reviewer@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "initial"]);
        fs::write(root.join("folder/visible.txt"), "changed\n").unwrap();

        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(
                store,
                ConfigFile::load(None),
                WorkspaceView::Git,
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(1280.), px(850.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.clone(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
                panel.show_history = false;
                cx.notify();
            });
        });
        visual.update(|_, cx| crate::workspace::apply_theme(gpui_component::ThemeMode::Dark, cx));
        for _ in 0..3 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }

        let slot = visual
            .debug_bounds("git-tree-stage:folder/visible.txt")
            .unwrap();
        let row = visual
            .debug_bounds("git-tree-row:file:folder/visible.txt")
            .unwrap();
        assert!(
            slot.size.width >= px(24.) && slot.size.height >= px(24.),
            "The staging slot should be at least 24×24: {slot:?}"
        );

        let colors = Palette::for_mode(gpui_component::ThemeMode::Dark);
        let target = gpui::point(slot.left() + px(2.), slot.center().y);
        let indicator = visual.update(|window, cx| {
            let scale = window.scale_factor();
            let expected = crate::workspace::file_name_font_size(cx);
            let indicator = window
                .painted_quads()
                .into_iter()
                .find(|quad| {
                    slot.scale(scale).contains(&quad.bounds.center())
                        && (quad.bounds.size.width.as_f32() / scale - expected).abs() <= 1.
                        && color_bytes(quad.border_color) == color_bytes(colors.control)
                })
                .expect("the checkbox is the same size as its filename and uses the control color");
            let inset = slot.right().as_f32() - indicator.bounds.right().as_f32() / scale;
            assert!(
                (inset - 6.).abs() <= 0.5,
                "staging indicator right inset: {inset}"
            );
            let row_inset = row.right().as_f32() - indicator.bounds.right().as_f32() / scale;
            assert!(
                (row_inset - 16.).abs() <= 0.5,
                "the checkbox must share the historical count's right edge: {row_inset}"
            );
            indicator
        });
        visual.simulate_mouse_move(target, None, gpui::Modifiers::default());
        visual.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
            let quads = window.painted_quads();
            let scale = window.scale_factor();
            let slot_quads: Vec<_> = quads.iter()
                .filter(|quad| slot.scale(scale).contains(&quad.bounds.center()))
                .map(|quad| (quad.bounds, quad.border_color, quad.background))
                .collect();
            assert!(
                quads.iter().any(|quad| {
                    quad.bounds == indicator.bounds
                        && quad.border_color != indicator.border_color
                        && quad.background.is_transparent()
                }),
                "hover changes the indicator border from {indicator:?} at {target:?}: {slot_quads:?}"
            );
            assert!(
                quads.iter().any(|quad| {
                    quad.bounds == indicator.bounds.dilate(px(2.).scale(scale))
                        && color_bytes(quad.border_color)
                            == color_bytes(colors.accent.opacity(0.72))
                        && quad.background.is_transparent()
                }),
                "hover draws a thin outer outline around the indicator: {slot_quads:?}"
            );
            assert!(
                !quads.iter().any(|quad| {
                    quad.bounds.contains(&target.scale(scale))
                        && quad.background == Background::from(colors.hover)
                }),
                "the staging hit area does not highlight the row or paint a background block"
            );
        });

        visual.simulate_click(target, gpui::Modifiers::default());
        visual.run_until_parked();
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            let review = panel.review.as_ref().unwrap();
            assert!(
                review.active.is_none(),
                "Clicking the staging slot should leave the file closed: {:?}",
                review.active
            );
            let statuses = review.detail.status.as_ref().unwrap();
            let status = &statuses["folder/visible.txt"];
            assert!(
                status.staged,
                "Clicking the empty area of the slot should stage the file"
            );
            assert!(!status.unstaged, "{status:?}");
        });
    }

    #[::core::prelude::v1::test]
    fn restores_worktree_session_across_windows() {
        use crate::{config::ConfigFile, repository::Repository};
        use gpui_component::input::{Position, RopeExt as _};

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        let other = fixture.path().join("other");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Reviewer"]);
        git(&root, &["config", "user.email", "reviewer@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        let source = PathBuf::from("code.txt");
        let document = PathBuf::from("guide.md");
        fs::write(root.join(&source), "line with text\n".repeat(160)).unwrap();
        fs::write(
            root.join(&document),
            "# Guide\n\n".to_owned() + &"A paragraph.\n\n".repeat(100),
        )
        .unwrap();
        fs::write(root.join("README.md"), "# Default document\n").unwrap();
        git(&root, &["add", "."]);
        for index in 0..76 {
            git(
                &root,
                &["commit", "--allow-empty", "-qm", &format!("Commit {index}")],
            );
        }
        git(
            &root,
            &["worktree", "add", "-qb", "other", other.to_str().unwrap()],
        );
        let repo = Repository::open(&root).unwrap();
        let other_repo = Repository::open(&other).unwrap();
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);

        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_startup_view(store.clone(), ConfigFile::load(None), None, window, cx)
        });
        visual.simulate_resize(size(px(1280.), px(850.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.clone(),
                        file: Some(source.clone()),
                    },
                    window,
                    cx,
                );
            })
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let editor = workspace.read(cx).files[&source].editor.clone();
            editor.update(cx, |editor, cx| {
                let offset = editor.text().position_to_offset(&Position::new(50, 2));
                editor.set_selected_range(offset..offset + 4, cx);
                editor.set_scroll_offset(point(px(0.), px(-920.)), cx);
            });
            window.draw(cx).clear(cx);
        });
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.switch_view(WorkspaceView::Docs, window, cx);
                workspace.open_doc(document.clone(), None, window, cx);
            })
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        visual.update(|_, cx| {
            workspace.update(cx, |workspace, cx| {
                let doc = workspace.doc.as_ref().unwrap();
                let mut position = doc.read(cx).session_position(cx).unwrap();
                position.block = 60;
                position.source_offset = None;
                position.inset = 7.;
                position.line = None;
                doc.update(cx, |doc, cx| {
                    doc.restore_position(Some(position));
                    cx.notify();
                });
                cx.notify();
            })
        });
        for _ in 0..3 {
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.simulate_next_frame(cx);
            });
            visual.run_until_parked();
        }
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.switch_view(WorkspaceView::Git, window, cx);
            })
        });
        visual.run_until_parked();
        let panel = visual.update(|window, cx| {
            let panel = workspace.read(cx).git.clone().unwrap();
            assert_eq!(
                panel.read(cx).selection,
                Some(Selection::Worktree(Mode::All))
            );
            panel.update(cx, |panel, cx| panel.load_commits(true, window, cx));
            panel
        });
        visual.run_until_parked();
        let selection = visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            panel.update(cx, |panel, cx| {
                let selection = Selection::Commit(panel.commits[66].oid.to_string(), None);
                panel.load_review(selection.clone(), false, window, cx);
                panel.history_scroll.scroll_to(ListOffset {
                    item_ix: 64,
                    offset_in_item: px(6.),
                });
                selection
            })
        });
        visual.run_until_parked();
        let executor = visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            cx.background_executor().clone()
        });
        executor.advance_clock(Duration::from_millis(750));
        visual.run_until_parked();
        assert_eq!(store.session(&repo).unwrap().view, Some(WorkspaceView::Git));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: other.clone(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        let saved = store.session(&repo).unwrap();
        assert_eq!(saved.view, Some(WorkspaceView::Git));
        assert_eq!(saved.tabs.active(), Some(source.as_path()));
        assert_eq!(saved.files[&source].selection_start, (50, 2));
        assert!(saved.files[&source].top.0 > 20);
        assert_eq!(saved.document.as_ref(), Some(&document));
        assert_eq!(saved.documents[&document].block, 60);
        let git_state = saved.git.unwrap();
        assert_eq!(git_state.selection.as_ref(), Some(&selection));
        let history = git_state.history.unwrap();
        assert!(history.oid.is_some());
        assert_eq!(store.session(&other_repo).unwrap().tabs, Default::default());

        let (restored, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_startup_view(store.clone(), ConfigFile::load(None), None, window, cx)
        });
        visual.simulate_resize(size(px(1280.), px(850.)));
        visual.update(|window, cx| {
            restored.update(cx, |workspace, cx| {
                workspace.open_repository(Location { root, file: None }, window, cx);
            })
        });
        for _ in 0..3 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        visual.update(|window, cx| {
            restored.update(cx, |workspace, cx| {
                assert_eq!(workspace.view, WorkspaceView::Git);
                let panel = workspace.git.as_ref().unwrap().read(cx);
                assert_eq!(panel.selection, Some(selection));
                assert_eq!(panel.session(cx).unwrap().history, Some(history));
                workspace.switch_view(WorkspaceView::Editor, window, cx);
            })
        });
        for _ in 0..3 {
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.simulate_next_frame(cx);
            });
        }
        visual.update(|window, cx| {
            restored.update(cx, |workspace, cx| {
                let file = &workspace.files[&source];
                assert!(file.restore_position.is_none());
                let editor = file.editor.read(cx);
                let (top, inset) = editor.viewport_anchor().unwrap();
                assert_eq!((top.line, top.character), saved.files[&source].top);
                assert_eq!(f32::from(inset), saved.files[&source].inset);
                let selection = editor.selected_range();
                let start = editor.text().offset_to_position(selection.start);
                let end = editor.text().offset_to_position(selection.end);
                assert_eq!(
                    (start.line, start.character),
                    saved.files[&source].selection_start
                );
                assert_eq!(
                    (end.line, end.character),
                    saved.files[&source].selection_end
                );
                workspace.switch_view(WorkspaceView::Docs, window, cx);
            })
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.simulate_next_frame(cx);
            });
        }
        visual.update(|_, cx| {
            let doc = restored.read(cx).doc.as_ref().unwrap().read(cx);
            assert_eq!(doc.path, document);
            assert_eq!(
                doc.session_position(cx).unwrap(),
                saved.documents[&document]
            );
        });
    }

    #[::core::prelude::v1::test]
    fn reviewing_a_worktree_loads_patches_and_updates_the_index() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        fs::write(root.join("review.txt"), "searchable change\n").unwrap();
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(
                store,
                crate::config::ConfigFile::load(None),
                WorkspaceView::Git,
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(1280.), px(850.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.clone(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
        for _ in 0..4 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        let toolbar = visual.debug_bounds("git-toolbar").unwrap();
        assert_eq!(toolbar.size.height, crate::workspace::FILE_TOOLBAR_HEIGHT);
        for selector in ["git-history-toolbar", "git-tree-toolbar"] {
            let bounds = visual.debug_bounds(selector).unwrap();
            assert_eq!(bounds.top(), toolbar.top());
            assert_eq!(bounds.bottom(), toolbar.bottom());
        }
        let disclosure = visual.debug_bounds("git-editor-disclosure").unwrap();
        assert_eq!(disclosure.size, size(px(20.), px(20.)));
        visual.simulate_click(disclosure.center(), Modifiers::default());
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.update(|_, cx| {
            assert!(panel.read(cx).diff.read(cx).is_file_collapsed("review.txt"));
        });
        let disclosure = visual.debug_bounds("git-editor-disclosure").unwrap();
        visual.simulate_click(disclosure.center(), Modifiers::default());
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.update(|_, cx| {
            assert!(!panel.read(cx).diff.read(cx).is_file_collapsed("review.txt"));
        });
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            let review = panel.review.as_ref().unwrap();
            assert_eq!(review.detail.files[0].path, "review.txt");
            assert!(matches!(review.patches[0], PatchState::Ready));
            assert_eq!(review.counts[&0], (1, 0));
        });

        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.find(window, cx);
                panel
                    .editor
                    .focused_column(window, cx)
                    .update(cx, |editor, cx| {
                        editor.set_search_query("searchable", true, cx);
                        assert!(editor.next_search_match(cx).is_some());
                        editor.close_search(cx);
                    });
                panel.stage_files(vec!["review.txt".into()], true, window, cx);
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                assert!(panel.stage_error.is_none(), "{:?}", panel.stage_error);
                let review = panel.review.as_ref().unwrap();
                assert!(review.detail.status.as_ref().unwrap()["review.txt"].staged);
                panel.load_review(Selection::Worktree(Mode::Staged), false, window, cx);
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                assert_eq!(panel.review.as_ref().unwrap().detail.files[0].code, 'A');
                panel.stage_files(vec!["review.txt".into()], false, window, cx);
            });
        });
        visual.run_until_parked();
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            assert!(panel.stage_error.is_none(), "{:?}", panel.stage_error);
            assert!(panel.review.as_ref().unwrap().detail.files.is_empty());
        });
        assert_eq!(
            fs::read_to_string(root.join("review.txt")).unwrap(),
            "searchable change\n"
        );
    }
    #[::core::prelude::v1::test]
    fn git_tree_checkbox_stages_all_descendants_even_when_filtered() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir_all(root.join("folder/nested")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Reviewer"]);
        git(&root, &["config", "user.email", "reviewer@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        let visible_path = "folder/visible.txt";
        let hidden_path = "folder/nested/hidden.txt";
        fs::write(root.join(visible_path), "original\n").unwrap();
        fs::write(root.join(hidden_path), "original\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "initial"]);
        fs::write(root.join(visible_path), "staged\n").unwrap();
        git(&root, &["add", visible_path]);
        fs::write(root.join(visible_path), "fully staged after click\n").unwrap();
        fs::write(root.join(hidden_path), "hidden change\n").unwrap();
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(
                store,
                crate::config::ConfigFile::load(None),
                WorkspaceView::Git,
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(1280.), px(850.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.clone(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
                panel.show_history = false;
                panel.filter = "visible".into();
                cx.notify();
            });
        });
        visual.run_until_parked();
        for expected_staged in [true, false] {
            for _ in 0..3 {
                visual.update(|window, cx| window.draw(cx).clear(cx));
                visual.run_until_parked();
            }
            let folder = visual.debug_bounds("git-tree-stage:folder").unwrap();
            let file = visual
                .debug_bounds("git-tree-stage:folder/visible.txt")
                .unwrap();
            assert_eq!(folder.right(), file.right());
            let checkbox = point(folder.right() - px(7.), folder.center().y);
            visual.simulate_mouse_move(checkbox, None, Modifiers::default());
            visual.simulate_click(checkbox, Modifiers::default());
            visual.run_until_parked();
            visual.update(|_, cx| {
                let panel = panel.read(cx);
                let review = panel.review.as_ref().unwrap();
                assert!(panel.stage_error.is_none(), "{:?}", panel.stage_error);
                assert!(!review.directories.contains("folder"));
                let statuses = review.detail.status.as_ref().unwrap();
                for path in [visible_path, hidden_path] {
                    assert_eq!(statuses[path].staged, expected_staged, "{path}");
                    assert_ne!(statuses[path].unstaged, expected_staged, "{path}");
                }
            });
        }
    }

    #[::core::prelude::v1::test]
    fn diff_hunk_toolbar_stages_and_unstages_in_both_layouts() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Reviewer"]);
        git(&root, &["config", "user.email", "reviewer@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        let path = "review.txt";
        fs::write(root.join(path), "before\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "initial"]);
        let changed = format!("{}\n", "long added line ".repeat(150));
        fs::write(root.join(path), &changed).unwrap();
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let config_path = fixture.path().join("config.toml");
        fs::write(&config_path, "[editor]\nfont_size = 20\n").unwrap();
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(
                store,
                crate::config::ConfigFile::load(Some(config_path)),
                WorkspaceView::Git,
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(1280.), px(850.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.clone(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.show_history = false;
                panel.wrap = false;
                panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.split = false;
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        visual.update(|window, cx| {
            let editor = panel.read(cx).editor.focused_column(window, cx).clone();
            assert_eq!(editor.read(cx).line_height(), Some(px(30.)));
            workspace.update(cx, |workspace, cx| {
                let mut config = workspace.config.current().clone();
                config.editor.line_height = 2.2;
                workspace.commit_settings(config, window, cx).unwrap();
            });
            window.draw(cx).clear(cx);
            assert_eq!(editor.read(cx).line_height(), Some(px(44.)));
        });
        let line = DiffLinePosition::new(path, gpui_component::diff::DiffSide::Modified, 1);
        let edit_point = visual.update(|_, cx| {
            let row = panel.read(cx).review_source_bounds(&line, cx).unwrap();
            point(row.left() + px(200.), row.center().y)
        });
        visual.simulate_mouse_move(edit_point, None, Modifiers::default());
        visual.simulate_click(edit_point, Modifiers::default());
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(visual.debug_bounds("git-diff-editor").is_some());
        visual.simulate_input("edited before ");
        visual.run_until_parked();
        let source = PathBuf::from(path);
        visual.update(|window, cx| {
            let editor = panel.read(cx).editor.focused_column(window, cx).clone();
            editor.update(cx, |editor, cx| {
                editor.set_search_query("edited", true, cx);
                assert!(editor.next_search_match(cx).is_some());
                editor.set_search_query("before", true, cx);
                let deleted = editor.text().to_string().find("before").unwrap();
                if editor.next_search_match(cx).unwrap().start != deleted {
                    assert_eq!(editor.next_search_match(cx).unwrap().start, deleted);
                }
                assert!(!editor.replace_current_search_match("blocked", window, cx));
                editor.close_search(cx);
            });
        });
        visual.update(|_, cx| {
            let file = &workspace.read(cx).files[&source];
            assert!(file.editor.read(cx).text().to_string().contains("edited "));
            assert!(file.is_dirty());
        });
        visual.update(|window, cx| {
            let editor = panel.read(cx).editor.focused_column(window, cx).clone();
            editor.update(cx, |editor, cx| {
                editor.set_search_query("before", true, cx);
                assert_eq!(editor.replace_all_search_matches("after", window, cx), 1);
                assert!(editor.text().to_string().contains("\nbefore\n"));
                assert!(editor.text().to_string().contains("edited after "));
                editor.close_search(cx);
            });
        });
        visual.run_until_parked();
        visual.update(|_, cx| {
            assert!(
                workspace.read(cx).files[&source]
                    .committed
                    .to_string()
                    .contains("edited after ")
            );
        });
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_document_source(source.clone(), window, cx);
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let file = &workspace.read(cx).files[&source];
            assert_eq!(file.editor.read(cx).line_height(), Some(px(44.)));
            for _ in 0..2 {
                window.dispatch_action(Box::new(gpui_component::input::Undo), cx);
            }
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            assert_eq!(
                workspace.read(cx).files[&source]
                    .editor
                    .read(cx)
                    .text()
                    .to_string(),
                changed
            );
            workspace.update(cx, |workspace, cx| {
                workspace.switch_view(WorkspaceView::Git, window, cx)
            });
        });
        visual.run_until_parked();
        for (mode, selector, expected_staged) in [
            (DiffMode::Unified, "git-hunk-action:0", true),
            (DiffMode::Split, "git-hunk-action:1", false),
        ] {
            visual.update(|_, cx| {
                panel.update(cx, |panel, cx| {
                    panel.split = mode == DiffMode::Split;
                    cx.notify();
                });
            });
            for _ in 0..4 {
                visual.update(|window, cx| window.draw(cx).clear(cx));
                visual.run_until_parked();
            }
            visual.update(|window, cx| {
                let diff = panel.read(cx).diff.clone();
                diff.update(cx, |diff, cx| {
                    diff.scroll_to_line(
                        DiffLinePosition::new(path, gpui_component::diff::DiffSide::Modified, 1),
                        cx,
                    );
                });
                diff.focus_handle(cx).focus(window, cx);
                window.draw(cx).clear(cx);
            });
            visual.simulate_mouse_move(point(px(2.), px(2.)), None, Modifiers::default());
            visual.update(|window, cx| window.draw(cx).clear(cx));
            assert!(visual.debug_bounds(selector).is_none());
            if mode == DiffMode::Split {
                visual.update(|window, cx| {
                    let editor = &panel.read(cx).editor;
                    assert_eq!(
                        editor.focused_column(window, cx).read(cx).line_height(),
                        Some(px(44.))
                    );
                    assert_eq!(
                        editor.original_state().read(cx).line_height(),
                        Some(px(44.))
                    );
                });
                let old_position =
                    DiffLinePosition::new(path, gpui_component::diff::DiffSide::Original, 1);
                let old = visual.update(|_, cx| {
                    panel
                        .read(cx)
                        .review_source_bounds(&old_position, cx)
                        .unwrap()
                });
                let click = point(old.left() + px(58.), old.center().y);
                visual.simulate_mouse_move(click, None, Modifiers::default());
                visual.simulate_click(click, Modifiers::default());
                visual.simulate_keystrokes("home shift-right");
                visual.update(|window, cx| {
                    let editor = panel.read(cx).editor.original_state().read(cx);
                    assert!(editor.focus_handle(cx).is_focused(window));
                    assert!(!editor.selected_range().is_empty());
                    assert!(editor.cursor_layout().is_some());
                });
                visual.simulate_input("x");
                visual.update(|_, cx| {
                    assert_eq!(
                        workspace.read(cx).files[&source]
                            .editor
                            .read(cx)
                            .text()
                            .to_string(),
                        changed
                    );
                });
                for _ in 0..5 {
                    visual.update(|window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.wrap = true;
                            cx.notify();
                        });
                        window.draw(cx).clear(cx);
                    });
                    visual.run_until_parked();
                }
                let (left, right) = visual.update(|_, cx| {
                    let panel = panel.read(cx);
                    (
                        panel.review_source_bounds(&old_position, cx).unwrap(),
                        panel.review_source_bounds(&line, cx).unwrap(),
                    )
                });
                assert_eq!(left.top(), right.top());
                assert_eq!(left.size.height, right.size.height);
                visual.update(|_, cx| {
                    let editor = panel.read(cx).editor.original_state().read(cx);
                    assert!(
                        right.size.height > editor.line_height().unwrap(),
                        "the long right line wraps"
                    );
                });
                visual.update(|_, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.wrap = false;
                        cx.notify();
                    });
                });
                for _ in 0..3 {
                    visual.update(|window, cx| window.draw(cx).clear(cx));
                    visual.run_until_parked();
                }
            }
            let hover = visual.update(|_, cx| {
                let row = panel
                    .read(cx)
                    .review_source_bounds(
                        &DiffLinePosition::new(path, gpui_component::diff::DiffSide::Modified, 1),
                        cx,
                    )
                    .unwrap();
                point(row.left() + px(40.), row.center().y)
            });
            visual.simulate_mouse_move(hover, None, Modifiers::default());
            visual.update(|window, cx| window.draw(cx).clear(cx));
            let before_scroll = visual.debug_bounds(selector).unwrap();
            let first_row = visual.update(|_, cx| {
                let position =
                    DiffLinePosition::new(path, gpui_component::diff::DiffSide::Original, 1);
                panel.read(cx).review_source_bounds(&position, cx).unwrap()
            });
            let label = visual.debug_bounds("editor-git-hunk-controls").unwrap();
            assert_eq!(
                label.size.height,
                gpui_component::input::HUNK_TOOLBAR_HEIGHT
            );
            assert!((label.top() - first_row.top()).abs() < px(1.));
            let viewport = visual.debug_bounds("git-diff-editor").unwrap();
            assert!(label.right() < viewport.right() - Scrollbar::width());
            visual.simulate_keystrokes("right right right");
            visual.update(|window, cx| window.draw(cx).clear(cx));
            let after_scroll = visual.debug_bounds(selector).unwrap();
            assert_eq!(before_scroll.right(), after_scroll.right());
            let action = after_scroll.center();
            visual.simulate_mouse_move(action, None, Modifiers::default());
            visual.simulate_click(action, Modifiers::default());
            visual.run_until_parked();
            visual.update(|_, cx| {
                let panel = panel.read(cx);
                let status = &panel
                    .review
                    .as_ref()
                    .unwrap()
                    .detail
                    .status
                    .as_ref()
                    .unwrap()[path];
                assert!(panel.stage_error.is_none(), "{:?}", panel.stage_error);
                assert_eq!(status.staged, expected_staged);
                assert_ne!(status.unstaged, expected_staged);
            });
            assert_eq!(fs::read_to_string(root.join(path)).unwrap(), changed);
        }
        assert!(
            !root.join(".git/stroll/edit-operations.json").exists(),
            "staging and unstaging hunks must not create operation history"
        );
        for _ in 0..4 {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            visual.run_until_parked();
        }
        let hover = visual.update(|_, cx| {
            let row = panel.read(cx).review_source_bounds(&line, cx).unwrap();
            point(row.right() - px(60.), row.center().y)
        });
        visual.simulate_mouse_move(hover, None, Modifiers::default());
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let restore = visual.debug_bounds("git-hunk-action:2").unwrap();
        visual.simulate_mouse_move(restore.center(), None, Modifiers::default());
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let bounds = restore.scale(window.scale_factor());
            let quads = window.painted_quads();
            let hover = quads
                .iter()
                .find(|quad| {
                    quad.bounds == bounds
                        && quad.background.as_solid().is_some_and(|color| color.a > 0.)
                })
                .expect("the restore button paints a hover background");
            assert_eq!(hover.corner_radii.bottom_right.as_f32(), 0.);
        });
        visual.simulate_click(restore.center(), Modifiers::default());
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), "before\n");
        visual.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).files[&source]
                    .editor
                    .read(cx)
                    .text()
                    .to_string(),
                "before\n"
            );
            assert!(!workspace.read(cx).files[&source].is_dirty());
        });
        let (undo, redo) = if cfg!(target_os = "macos") {
            ("cmd-z", "cmd-shift-z")
        } else {
            ("ctrl-z", "ctrl-shift-z")
        };
        // The restored file disappeared from the diff, so the review keeps worktree focus.
        visual.simulate_keystrokes(undo);
        visual.run_until_parked();
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), "before\n");
        visual.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).files[&source]
                    .editor
                    .read(cx)
                    .text()
                    .to_string(),
                changed
            );
            assert!(workspace.read(cx).files[&source].is_dirty());
        });
        visual.simulate_keystrokes(redo);
        visual.run_until_parked();
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), "before\n");

        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_document_source(source.clone(), window, cx)
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.simulate_input("X");
        visual.run_until_parked();
        visual.update(|_, cx| assert!(workspace.read(cx).files[&source].is_dirty()));
        visual.simulate_keystrokes(undo);
        visual.run_until_parked();
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), "before\n");
        visual.update(|_, cx| {
            let file = &workspace.read(cx).files[&source];
            assert_eq!(file.committed, file.saved);
        });
        visual.simulate_keystrokes(undo);
        visual.run_until_parked();
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), "before\n");
        visual.simulate_keystrokes(undo);
        visual.run_until_parked();
        let staged = visual.update(|_, cx| {
            panel
                .read(cx)
                .backend
                .file_version(path)
                .unwrap()
                .status
                .is_some_and(|status| status.staged)
        });
        assert!(!staged, "index edits must not be replayed by editor undo");
        visual.update(|_, cx| {
            let file = &workspace.read(cx).files[&source];
            assert_eq!(file.committed.to_string(), changed);
        });
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), "before\n");
    }

    #[::core::prelude::v1::test]
    fn worktree_row_and_review_summary_scroll_with_their_content() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Reviewer"]);
        git(&root, &["config", "user.email", "reviewer@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("review.txt"), "before\n").unwrap();
        git(&root, &["add", "."]);
        for ix in 0..16 {
            git(
                &root,
                &["commit", "--allow-empty", "-qm", &format!("Commit {ix}")],
            );
        }
        fs::write(root.join("review.txt"), "after\n".repeat(140)).unwrap();
        fs::write(root.join("second.txt"), "another change\n".repeat(140)).unwrap();
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(
                store,
                crate::config::ConfigFile::load(None),
                WorkspaceView::Git,
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(1280.), px(850.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.clone(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        let panel = visual.update(|window, cx| {
            let panel = workspace.read(cx).git.clone().unwrap();
            panel.update(cx, |panel, cx| panel.select_worktree(window, cx));
            gpui_component::Theme::set_scrollbar_mode(
                gpui_component::scroll::ScrollbarMode::Always,
                cx,
            );
            cx.set_reduce_motion(true);
            panel
        });
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        let scroll = |position, distance| ScrollEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.), px(distance))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        };
        let (history, diff) = visual.update(|_, cx| {
            let panel = panel.read(cx);
            assert_eq!(panel.history_scroll.item_count(), panel.commits.len() + 1);
            assert_eq!(panel.history_scroll.logical_scroll_top().item_ix, 0);
            let content_height = panel.history_scroll.max_offset_for_scrollbar().y
                + panel.history_scroll.viewport_bounds().size.height;
            assert!((content_height - px(54. + 66. + 15. * 54.)).abs() <= px(1.));
            assert!(panel.diff.read(cx).top_file().is_none());
            let history = panel.history_scroll.viewport_bounds().center();
            (history, point(px(600.), history.y))
        });
        visual.simulate_event(scroll(history, -240.));
        visual.simulate_event(scroll(diff, -240.));
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let panel = panel.read(cx);
            assert!(panel.history_scroll.logical_scroll_top().item_ix > 0);
            assert_eq!(
                panel.diff.read(cx).top_file().map(SharedString::as_str),
                Some("review.txt")
            );
        });
        #[cfg(debug_assertions)]
        {
            // Keep the pointer inside the same change across fractional scroll
            // offsets. The label sticks below the current file header.
            let editor_bounds = visual.debug_bounds("git-diff-editor").unwrap();
            let hover = point(editor_bounds.center().x, editor_bounds.top() + px(120.));
            visual.simulate_mouse_move(hover, None, Modifiers::default());
            let mut previous = None;
            for distance in [-67.5, -16.25, -21.5] {
                visual.simulate_event(scroll(hover, distance));
                let row = visual.update(|window, cx| {
                    window.draw(cx).clear(cx);
                    let row = panel
                        .read(cx)
                        .review_source_bounds(
                            &DiffLinePosition::new(
                                "review.txt",
                                gpui_component::diff::DiffSide::Modified,
                                23,
                            ),
                            cx,
                        )
                        .unwrap();
                    let color = Background::from(
                        crate::workspace::code_theme::HunkPalette::for_mode(cx.theme().mode)
                            .added_row,
                    );
                    let scale = window.scale_factor();
                    let device_pixel = px(1. / scale);
                    assert!(window.painted_quads().iter().any(|quad| {
                        quad.background == color
                            && (px(quad.bounds.top().0 / scale) - row.top()).abs() <= device_pixel
                            && (px(quad.bounds.size.height.0 / scale) - row.size.height).abs()
                                <= device_pixel
                    }));
                    row
                });
                if let Some((last_row, last_label)) = previous {
                    assert!(row.top() < last_row);
                    assert_eq!(
                        visual
                            .debug_bounds("editor-git-hunk-controls")
                            .unwrap()
                            .top(),
                        last_label
                    );
                }
                previous = Some((
                    row.top(),
                    visual
                        .debug_bounds("editor-git-hunk-controls")
                        .unwrap()
                        .top(),
                ));
            }
            visual.simulate_mouse_move(point(px(2.), px(2.)), None, Modifiers::default());
            visual.update(|window, cx| window.draw(cx).clear(cx));
            assert!(visual.debug_bounds("editor-git-hunk-controls").is_none());
        }
        visual.simulate_event(scroll(diff, 500.));
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(panel.read(cx).diff.read(cx).top_file().is_none());
        });

        let diff_state = visual.update(|_, cx| panel.read(cx).diff.clone());
        for _ in 0..3 {
            visual.update(|window, cx| {
                diff_state.update(cx, |state, cx| state.scroll_to_file("second.txt", cx));
                window.draw(cx).clear(cx);
            });
            visual.run_until_parked();
        }
        #[cfg(debug_assertions)]
        {
            // The last file's header has no next header to push it away, so it
            // must stay pinned at the viewport top as soon as its row starts
            // scrolling out, instead of jumping in after the row is gone.
            let editor_top = visual.debug_bounds("git-diff-editor").unwrap().top();
            for _ in 0..2 {
                visual.simulate_event(scroll(diff, -12.));
                visual.update(|window, cx| window.draw(cx).clear(cx));
                let header = visual
                    .debug_bounds("git-editor-file-header:second.txt")
                    .expect("the sticky file header stays visible while its row scrolls out");
                assert_eq!(header.top(), editor_top);
                assert_eq!(header.size.height, px(32.));
            }
        }
        visual.simulate_event(scroll(diff, 12.));
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                panel
                    .read(cx)
                    .review_source_bounds(
                        &DiffLinePosition::new(
                            "second.txt",
                            gpui_component::diff::DiffSide::Modified,
                            1
                        ),
                        cx,
                    )
                    .is_some()
            );
            diff_state.update(cx, |state, cx| state.scroll_to_file("review.txt", cx));
            window.draw(cx).clear(cx);
        });

        visual.simulate_event(scroll(diff, -240.));
        visual.update(|window, cx| window.draw(cx).clear(cx));
        #[cfg(debug_assertions)]
        {
            let editor_button = visual
                .debug_bounds("git-open-file-in-editor:review.txt")
                .unwrap()
                .center();
            visual.simulate_mouse_move(editor_button, None, Modifiers::default());
            let git_session = visual.update(|_, cx| panel.read(cx).session(cx).unwrap());
            visual.simulate_click(editor_button, Modifiers::default());
            visual.run_until_parked();
            visual.update(|_, cx| {
                let workspace = workspace.read(cx);
                assert!(workspace.view == WorkspaceView::Editor);
                assert_eq!(workspace.tabs.active(), Some(Path::new("review.txt")));
                assert_eq!(workspace.session.git.as_ref(), Some(&git_session));
            });
        }
    }
    #[::core::prelude::v1::test]
    #[cfg(debug_assertions)]
    fn diff_scrollbar_paints_above_the_header_overlay() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Reviewer"]);
        git(&root, &["config", "user.email", "reviewer@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("review.txt"), "before\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "base"]);
        fs::write(root.join("review.txt"), "after\n".repeat(140)).unwrap();
        fs::write(root.join("second.txt"), "another change\n".repeat(140)).unwrap();
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(
                store,
                crate::config::ConfigFile::load(None),
                WorkspaceView::Git,
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(1280.), px(850.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.clone(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        let panel = visual.update(|window, cx| {
            let panel = workspace.read(cx).git.clone().unwrap();
            panel.update(cx, |panel, cx| panel.select_worktree(window, cx));
            // Diff content keeps its scrollbar visible without a theme-wide
            // override. Reduced motion keeps the frame deterministic.
            cx.set_reduce_motion(true);
            panel
        });
        for _ in 0..6 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        let diff = visual.update(|_, cx| panel.read(cx).diff.clone());
        for _ in 0..3 {
            visual.update(|window, cx| {
                diff.update(cx, |state, cx| state.scroll_to_file("review.txt", cx));
                window.draw(cx).clear(cx);
            });
            visual.run_until_parked();
        }
        let scroll = |distance: f32| ScrollEvent {
            position: point(px(600.), px(400.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(distance))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        };
        let thumb = |window: &Window, viewport: Bounds<Pixels>| {
            let scale = window.scale_factor();
            let quads = window.painted_quads();
            let index = quads
                .iter()
                .position(|quad| {
                    let bounds = quad.bounds;
                    let width = bounds.size.width.as_f32() / scale;
                    let height = bounds.size.height.as_f32() / scale;
                    let right = bounds.right().as_f32() / scale;
                    (4. ..=20.).contains(&width)
                        && (20. ..=400.).contains(&height)
                        && right <= viewport.right().as_f32() + 0.5
                        && right > viewport.right().as_f32() - 12.
                        && quad
                            .background
                            .as_solid()
                            .is_some_and(|color| color.a > 0.5)
                })
                .expect("a recent scroll reveals the vertical scrollbar thumb");
            (index, quads[index].bounds, quads)
        };

        // The review summary and every visible file header are painted in an
        // overlay canvas above the editor. At the top of the diff that overlay
        // covers the scrollbar's column, so the thumb must still paint last.
        visual.simulate_event(scroll(4000.));
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let viewport = visual.debug_bounds("git-diff-editor").unwrap();
        let (thumb_index, thumb_bounds, quads) = visual.update(|window, _| thumb(window, viewport));
        assert!(
            thumb_bounds.top().as_f32() / visual.update(|window, _| window.scale_factor())
                < viewport.top().as_f32() + 40.,
            "the thumb sits in the overlay band at the top of the diff"
        );
        let mut covered = false;
        for (index, quad) in quads.iter().enumerate() {
            if index == thumb_index
                || !quad.bounds.intersects(&thumb_bounds)
                || !quad
                    .background
                    .as_solid()
                    .is_some_and(|color| color.a > 0.5)
            {
                continue;
            }
            assert!(
                index < thumb_index,
                "quad {index} paints over the scrollbar thumb"
            );
            covered = true;
        }
        assert!(covered, "the overlay should cross the thumb's column");

        // A pinned file header comes from that same overlay. Once it is stuck
        // at the viewport top the scrollbar has to paint above it as well.
        for _ in 0..3 {
            visual.update(|window, cx| {
                diff.update(cx, |state, cx| state.scroll_to_file("second.txt", cx));
                window.draw(cx).clear(cx);
            });
            visual.run_until_parked();
        }
        visual.simulate_event(scroll(-12.));
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let header = visual
            .debug_bounds("git-editor-file-header:second.txt")
            .expect("the last file header stays pinned");
        let scale = visual.update(|window, _| window.scale_factor());
        let header_bounds = Bounds {
            origin: header.origin.scale(scale),
            size: header.size.scale(scale),
        };
        let (thumb_index, _, quads) = visual.update(|window, _| thumb(window, viewport));
        let header_index = quads
            .iter()
            .position(|quad| {
                quad.bounds == header_bounds
                    && quad
                        .background
                        .as_solid()
                        .is_some_and(|color| color.a > 0.5)
            })
            .expect("the pinned file header paints an opaque row");
        assert!(
            header_index < thumb_index,
            "the scrollbar must paint above the pinned file header"
        );
    }

    #[::core::prelude::v1::test]
    fn review_image_hover_preserves_diff_layout() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-qb", "main"]);
        let picture = root.join("image.png");
        let text = root.join("source.txt");
        image::RgbImage::from_pixel(96, 96, image::Rgb([32, 160, 144]))
            .save(&picture)
            .unwrap();
        fs::write(&text, "before\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Reviewer",
                "-c",
                "user.email=reviewer@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "Initial",
            ],
        );
        image::RgbImage::from_pixel(1920, 1080, image::Rgb([224, 120, 62]))
            .save(&picture)
            .unwrap();
        fs::write(&text, "after\n").unwrap();

        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view =
                cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Git, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1180.), px(780.)));
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                this.open_repository(Location { root, file: None }, window, cx)
            });
        });
        visual.run_until_parked();
        let panel = visual.update(|window, cx| {
            let panel = workspace.read(cx).git.clone().unwrap();
            panel.update(cx, |panel, cx| {
                panel.split = true;
                panel.show_history = false;
                panel.show_tree = false;
                panel.select_worktree(window, cx);
                cx.notify();
            });
            panel
        });
        let frame = |visual: &mut VisualTestContext| {
            for _ in 0..8 {
                visual.run_until_parked();
                visual.update(|window, cx| {
                    window.simulate_next_frame(cx);
                    window.draw(cx).clear(cx);
                });
            }
        };
        frame(visual);
        visual.update(|window, cx| {
            let panel = panel.read(cx);
            let review = panel.review.as_ref().unwrap();
            let sides = &review.images[&review.by_path["image.png"]];
            let [
                review::DiffImage::Ready(before),
                review::DiffImage::Ready(after),
            ] = sides
            else {
                panic!("both images should decode");
            };
            assert_eq!(before.image.size(0).width, before.image.size(0).height);
            let thumbnail = after.image.size(0);
            let scale = window.scale_factor();
            assert!(thumbnail.width > thumbnail.height);
            assert!(thumbnail.width.0 <= (160. * scale).ceil() as i32);
            assert!(thumbnail.height.0 <= (112. * scale).ceil() as i32);
        });
        let layout = |visual: &mut VisualTestContext| {
            [
                visual
                    .debug_bounds("git-image-thumbnail:before:image.png")
                    .unwrap(),
                visual
                    .debug_bounds("git-image-thumbnail:after:image.png")
                    .unwrap(),
                visual
                    .debug_bounds("git-editor-file-header:source.txt")
                    .unwrap(),
            ]
        };
        let viewport = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| panel.read(cx).diff.read(cx).editor_viewport_position())
        };
        let initial_viewport = viewport(visual);
        let before = layout(visual);
        let image_row = visual.debug_bounds("git-image-diff:image.png").unwrap();
        assert!((image_row.bottom() - before[2].top()).abs() < px(0.01));
        let arrow = visual.debug_bounds("git-image-arrow:image.png").unwrap();
        assert!(before[0].right() < arrow.left() && arrow.right() < before[1].left());
        let delay = visual
            .update(|_, cx| gpui_component::tooltip::TooltipDefaults::global(cx).show_delay());
        visual.simulate_mouse_move(before[1].center(), None, Modifiers::default());
        visual
            .executor()
            .advance_clock(delay + Duration::from_millis(32));
        frame(visual);
        let popup = visual
            .debug_bounds("image-hover-preview")
            .expect("image diff hover");
        assert_eq!(popup.size, size(px(320.), px(240.)));
        visual.update(|window, cx| {
            let scale = window.scale_factor();
            let expected = cx.theme().popover.alpha(0.5);
            assert!(window.painted_quads().iter().any(|quad| {
                quad.bounds.size == popup.size.scale(scale)
                    && (quad.bounds.left() - popup.left().scale(scale))
                        .as_f32()
                        .abs()
                        <= scale
                    && (quad.bounds.top() - popup.top().scale(scale))
                        .as_f32()
                        .abs()
                        <= scale
                    && quad.background.as_solid().is_some_and(|color| {
                        color.a > 0. && color.a <= 0.5 && color.alpha(0.5) == expected
                    })
            }));
        });
        assert!(visual.debug_bounds("image-hover-loaded").is_some());
        frame(visual);
        assert!(visual.debug_bounds("image-hover-loaded").is_some());
        assert_eq!(layout(visual), before);
        assert_eq!(viewport(visual), initial_viewport);
        visual.simulate_mouse_move(point(px(2.), px(2.)), None, Modifiers::default());
        std::thread::sleep(
            gpui_base::PopupMotionKind::Hover.duration(false) + Duration::from_millis(16),
        );
        frame(visual);
        assert!(visual.debug_bounds("image-hover-preview").is_none());
        assert_eq!(layout(visual), before);
        assert_eq!(viewport(visual), initial_viewport);

        visual.simulate_mouse_move(before[0].center(), None, Modifiers::default());
        visual
            .executor()
            .advance_clock(delay + Duration::from_millis(32));
        frame(visual);
        assert!(visual.debug_bounds("image-hover-loaded").is_some());
        visual.simulate_event(ScrollEvent {
            position: before[0].center(),
            delta: ScrollDelta::Lines(point(0., -2.)),
            source: ScrollSource::Pointer,
            touch_phase: TouchPhase::Moved,
            modifiers: Modifiers::default(),
        });
        frame(visual);
        assert!(visual.debug_bounds("image-hover-preview").is_none());
    }

    #[cfg(debug_assertions)]
    #[::core::prelude::v1::test]
    fn review_block_heights_follow_content_and_keep_split_code_aligned() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("code.txt"), "before\nunchanged\n").unwrap();
        fs::write(root.join("binary.bin"), b"\0before").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Reviewer",
                "-c",
                "user.email=reviewer@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "Initial",
            ],
        );
        let head = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                String::from_utf8_lossy(&head.stdout).trim(),
                "b-submodule",
            ],
        );
        fs::create_dir(root.join("b-submodule")).unwrap();
        fs::write(root.join("binary.bin"), b"\0after").unwrap();
        fs::write(root.join("bb-empty"), "").unwrap();
        fs::write(root.join("code.txt"), "after\nunchanged\n").unwrap();
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let config_path = fixture.path().join("config.toml");
        fs::write(&config_path, "[editor]\nfont_size = 24\n").unwrap();
        let config = crate::config::ConfigFile::load(Some(config_path));
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
        });
        visual.simulate_resize(size(px(1400.), px(850.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(Location { root, file: None }, window, cx);
            });
        });
        visual.run_until_parked();
        let panel = visual.update(|window, cx| {
            let panel = workspace.read(cx).git.clone().unwrap();
            panel.update(cx, |panel, cx| {
                panel.split = true;
                panel.wrap = true;
                panel.show_history = false;
                panel.show_tree = false;
                panel.select_worktree(window, cx);
                cx.notify();
            });
            panel
        });
        let settle = |visual: &mut VisualTestContext| {
            for _ in 0..4 {
                visual.run_until_parked();
                visual.update(|window, cx| window.draw(cx).clear(cx));
            }
        };
        settle(visual);
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                let review = panel.review.as_mut().unwrap();
                let sub = &review.detail.files[review.by_path["b-submodule"]];
                assert!(sub.submodule(), "{sub:?}");
                let ix = review.by_path["bb-empty"];
                let code = review.by_path["code.txt"];
                // The same notice renderer also handles backend diagnostics.
                review.patches[ix] = PatchState::Failed;
                review.patch_display[ix] = PatchDisplay {
                    truncated: None,
                    content: None,
                    error: Some("Unable to read the requested revision. ".repeat(14)),
                };
                panel.editor.invalidate();
                panel.request_patch(code, window, cx);
                App::notify(cx, panel.review_pane.entity_id());
            });
        });
        settle(visual);

        let check_layout = |visual: &mut VisualTestContext| {
            let mut bounds = |selector| {
                visual
                    .debug_bounds(selector)
                    .unwrap_or_else(|| panic!("missing {selector}"))
            };
            for (header, notice) in [
                (
                    "git-editor-file-header:b-submodule",
                    "git-editor-notice:b-submodule",
                ),
                (
                    "git-editor-file-header:binary.bin",
                    "git-editor-notice:binary.bin",
                ),
                (
                    "git-editor-file-header:bb-empty",
                    "git-editor-notice:bb-empty",
                ),
            ] {
                let header = bounds(header);
                let notice = bounds(notice);
                assert_eq!(header.size.height, px(32.));
                assert!((notice.top() - header.bottom()).abs() < px(0.01));
            }
            let binary = bounds("git-editor-notice:binary.bin");
            let code = bounds("git-editor-file-header:code.txt");
            assert!((binary.bottom() - code.top()).abs() < px(0.01));
            let diagnostic_height = bounds("git-editor-notice:bb-empty").size.height;

            visual.update(|_, cx| {
                let panel = panel.read(cx);
                assert_eq!(panel.diff.read(cx).mode(), DiffMode::Split);
                let row = |side| {
                    panel
                        .review_source_bounds(&DiffLinePosition::new("code.txt", side, 2), cx)
                        .unwrap()
                };
                let left = row(DiffSide::Original);
                let right = row(DiffSide::Modified);
                assert_eq!(left.top(), right.top());
                assert_eq!(left.size.height, px(36.));
                assert_eq!(right.size.height, left.size.height);
            });
            diagnostic_height
        };
        let wide = check_layout(visual);
        visual.simulate_resize(size(px(720.), px(850.)));
        settle(visual);
        assert!(
            check_layout(visual) > wide,
            "the diagnostic wraps at a narrower width"
        );
    }

    #[::core::prelude::v1::test]
    #[ignore = "manual release-mode Git scroll CPU benchmark"]
    fn git_scroll_cpu_budget() {
        use std::time::Instant;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Reviewer"]);
        git(&root, &["config", "user.email", "reviewer@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        for ix in 0..60 {
            git(
                &root,
                &["commit", "--allow-empty", "-qm", &format!("Commit {ix}")],
            );
        }
        // Directories sort before root files. Put the large file ahead of the cold range.
        fs::create_dir(root.join("large")).unwrap();
        fs::write(
            root.join("large/00-large.rs"),
            "let some_value = function_call(with_a_long_argument, and_another_argument);\n"
                .repeat(2600),
        )
        .unwrap();
        for dir in 0..30 {
            let dir = root.join(format!("src-{dir:02}")).join("nested");
            fs::create_dir_all(&dir).unwrap();
            for file in 0..20 {
                fs::write(dir.join(format!("{file:02}.rs")), "fn changed() {}\n").unwrap();
            }
        }

        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let (workspace, visual) = cx.add_window_view(|window, cx| {
            Workspace::with_config(
                store,
                crate::config::ConfigFile::load(None),
                WorkspaceView::Git,
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(1280.), px(850.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(Location { root, file: None }, window, cx);
            });
        });
        visual.run_until_parked();
        let panel = visual.update(|window, cx| {
            let panel = workspace.read(cx).git.clone().unwrap();
            panel.update(cx, |panel, cx| {
                panel.select_worktree(window, cx);
                panel.request_patch(0, window, cx);
            });
            panel
        });
        for _ in 0..6 {
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
            visual.run_until_parked();
        }
        let (history, diff, diff_state) = visual.update(|_, cx| {
            let panel = panel.read(cx);
            let history = panel.history_scroll.viewport_bounds().center();
            assert_eq!(panel.review.as_ref().unwrap().detail.files.len(), 601);
            (history, point(px(600.), history.y), panel.diff.clone())
        });
        for (label, position, distance) in [
            ("history", history, -30.),
            ("warm diff", diff, -45.),
            ("cold diff", diff, -420.),
        ] {
            if label == "cold diff" {
                visual.update(|_, cx| {
                    diff_state.update(cx, |state, cx| {
                        state.scroll_to_file("src-00/nested/00.rs", cx)
                    });
                });
                for _ in 0..3 {
                    visual.update(|window, cx| window.draw(cx).clear(cx));
                    visual.run_until_parked();
                }
                visual.update(|_, cx| {
                    let diff = diff_state.read(cx);
                    assert_eq!(
                        diff.top_file().map(SharedString::as_str),
                        Some("src-00/nested/00.rs")
                    );
                });
            }
            let mut frames = Vec::new();
            let mut interactions = Vec::new();
            let mut cold_top = 0;
            for _ in 0..100 {
                let event = ScrollEvent {
                    position,
                    delta: ScrollDelta::Pixels(point(px(0.), px(distance))),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Moved,
                    source: ScrollSource::Pointer,
                };
                let start = Instant::now();
                visual.update(|window, cx| {
                    window.dispatch_event(PlatformInput::Scroll(event), cx);
                });
                let dispatch_ms = start.elapsed().as_secs_f64() * 1000.;
                visual.executor().advance_clock(Duration::from_millis(17));
                let draw_ms = visual.update(|window, cx| {
                    let draw = Instant::now();
                    window.simulate_next_frame(cx);
                    window.draw(cx).clear(cx);
                    draw.elapsed().as_secs_f64() * 1000.
                });
                frames.push(draw_ms);
                interactions.push(dispatch_ms + draw_ms);
                visual.run_until_parked();
                if label == "cold diff" {
                    visual.update(|_, cx| {
                        let panel = panel.read(cx);
                        let diff = panel.diff.read(cx);
                        let top = diff.top_file().unwrap();
                        let ix = panel.review.as_ref().unwrap().by_path[top.as_str()];
                        assert!(ix >= cold_top, "cold scroll moved backward at {top}");
                        cold_top = ix;
                    });
                }
            }
            frames.sort_by(f64::total_cmp);
            interactions.sort_by(f64::total_cmp);
            let over_budget = frames.iter().filter(|&&ms| ms > 16.67).count();
            eprintln!(
                "{label}: draw p50={:.2} ms p95={:.2} ms >16.67ms={over_budget}/100, interaction p95={:.2} ms",
                frames[50], frames[95], interactions[95]
            );
        }
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            assert!(panel.history_scroll.logical_scroll_top().item_ix > 30);
            let top = panel.diff.read(cx).top_file().unwrap();
            assert!(
                top.as_str().starts_with("src-") && top.as_str() != "src-00/nested/00.rs",
                "cold scroll never advanced past its starting file: {top}"
            );
            assert!(
                panel
                    .review
                    .as_ref()
                    .unwrap()
                    .patches
                    .iter()
                    .filter(|state| matches!(state, PatchState::Ready))
                    .count()
                    > 12
            );
        });
    }
}

pub(super) fn status_color(code: char, colors: Palette, cx: &App) -> Hsla {
    match code {
        'A' | '?' => cx.theme().success,
        'D' | 'U' => cx.theme().danger,
        'R' | 'C' => colors.accent,
        _ => colors.warning,
    }
}
