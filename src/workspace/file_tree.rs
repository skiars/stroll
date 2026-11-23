use std::{cell::RefCell, io::Write, rc::Weak};

use gpui_component::{
    input::{Input, InputState},
    menu::{ContextMenuExt, PopupMenu},
};

use super::*;
use crate::repository::files::{CopyTarget, EntryVersion, FileOperation, valid_name};

const TREE_GUTTER: f32 = 15.;
const TREE_INDENT: f32 = 14.;
const MAX_TREE_DEPTH: usize = u16::BITS as usize;
pub(super) const TREE_ICON_SIZE: f32 = 15.;
pub(super) const TREE_LABEL_GAP: Pixels = px(5.);
pub(super) const TREE_END_PADDING: Pixels = px(8.);
const SIDE_BUTTON_SCROLL_STEP: Pixels = px(24.);
const HORIZONTAL_WHEEL_SCALE: f32 = 1. / 4.;

pub(super) fn tree_indent(depth: usize) -> Pixels {
    px(TREE_GUTTER + TREE_INDENT * depth.min(MAX_TREE_DEPTH) as f32)
}

#[derive(Default)]
pub(super) struct TreeHorizontalScroll {
    pub(super) handle: ScrollHandle,
    measured: RefCell<Option<TreeWidth>>,
}

struct TreeWidth {
    rows: Weak<Vec<TreeRow>>,
    font: Font,
    font_size: Pixels,
    width: Pixels,
}

impl TreeHorizontalScroll {
    pub(super) fn content_width(
        &self,
        rows: &Rc<Vec<TreeRow>>,
        window: &Window,
        cx: &App,
    ) -> Pixels {
        let theme = cx.theme();
        let mut font = font(theme.font_family.clone());
        font.fallbacks = theme.font_fallbacks.clone();
        let font_size = px(file_name_font_size(cx));
        let rows_key = Rc::downgrade(rows);
        let mut measured = self.measured.borrow_mut();
        if let Some(cached) = measured.as_ref()
            && cached.rows.ptr_eq(&rows_key)
            && cached.font == font
            && cached.font_size == font_size
        {
            return cached.width;
        }

        let text_system = window.text_system();
        let mut run = TextStyle::default().to_run(0);
        run.font = font.clone();
        let width = rows
            .iter()
            .map(|row| {
                let name = file_name(&row.entry.path);
                let label = if row.entry.directory && row.depth > 0 {
                    // The first sticky row may include a marker for omitted ancestors.
                    std::borrow::Cow::Owned(format!("…/{name}"))
                } else {
                    std::borrow::Cow::Borrowed(name)
                };
                run.len = label.len();
                run.font.weight = FontWeight::NORMAL;
                let regular = text_system.layout_line(&label, font_size, &[run.clone()], None);
                run.font.weight = FontWeight::SEMIBOLD;
                let selected = text_system.layout_line(&label, font_size, &[run.clone()], None);
                tree_indent(row.depth)
                    + px(TREE_ICON_SIZE)
                    + TREE_LABEL_GAP
                    + regular.width.max(selected.width).ceil()
                    + TREE_END_PADDING
            })
            .max()
            .unwrap_or(px(0.));
        *measured = Some(TreeWidth {
            rows: rows_key,
            font,
            font_size,
            width,
        });
        width
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct TreeGuides {
    full: u16,
    half: u16,
}

impl TreeGuides {
    pub(super) fn new(ancestors: u16, depth: usize, last: bool) -> Self {
        let branch = match depth {
            1..=MAX_TREE_DEPTH => 1 << (depth - 1),
            _ => 0,
        };
        if last {
            Self {
                full: ancestors,
                half: branch,
            }
        } else {
            Self {
                full: ancestors | branch,
                half: 0,
            }
        }
    }

    pub(super) fn continuing(self) -> u16 {
        self.full
    }

    pub(super) fn element(self, color: Hsla) -> Option<impl IntoElement> {
        (self.full | self.half != 0).then(|| {
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    for level in 0..MAX_TREE_DEPTH {
                        let bit = 1 << level;
                        if (self.full | self.half) & bit == 0 {
                            continue;
                        }
                        let height = if self.half & bit != 0 {
                            bounds.size.height / 2.
                        } else {
                            bounds.size.height
                        };
                        let x = bounds.left() + px(TREE_GUTTER + TREE_INDENT * level as f32 + 7.);
                        let mut y = px(0.);
                        while y < height {
                            let dash = Bounds::new(
                                point(x, bounds.top() + y),
                                size(px(1.), px(2.).min(height - y)),
                            );
                            window.paint_quad(fill(dash, color.opacity(0.35)));
                            y += px(4.);
                        }
                    }
                },
            )
            .absolute()
            .inset_0()
        })
    }
}

pub(super) struct TreeEdit {
    path: PathBuf,
    directory: bool,
    version: Option<EntryVersion>,
    input: Entity<InputState>,
    return_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

impl TreeEdit {
    pub(super) fn row(&self, rows: &[TreeRow]) -> Option<(usize, usize, bool)> {
        if self.version.is_some() {
            let index = rows.iter().position(|row| row.entry.path == self.path)?;
            Some((index, rows[index].depth, true))
        } else if self.path.as_os_str().is_empty() {
            Some((0, 0, false))
        } else {
            let index = rows.iter().position(|row| row.entry.path == self.path)?;
            Some((index + 1, rows[index].depth + 1, false))
        }
    }

    pub(super) fn element(
        &self,
        depth: usize,
        guides: TreeGuides,
        guide_color: Hsla,
        busy: bool,
        workspace: WeakEntity<Workspace>,
    ) -> AnyElement {
        h_flex()
            .relative()
            .h(tree_motion::ROW_HEIGHT)
            .w_full()
            .pl(tree_indent(depth))
            .pr_2()
            .on_key_down(move |event, window, cx| {
                if event.keystroke.key == "escape" && !busy {
                    cx.stop_propagation();
                    _ = workspace.update(cx, |this, cx| this.finish_tree_edit(window, cx));
                }
            })
            .children(guides.element(guide_color))
            .child(Input::new(&self.input).small().disabled(busy).w_full())
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
pub(super) enum TreeAction {
    Open(bool),
    OpenInDocs,
    OpenSource,
    PreviewAudio,
    SaveCopy,
    Create(bool),
    CopyPath(bool),
    CopyEntry(bool),
    PasteEntry,
    Rename,
    Delete,
    Refresh,
    Collapse,
}

actions!(workspace, [RenameTreeEntry, DeleteTreeEntry, RefreshTree]);

#[derive(Action, Clone, PartialEq)]
#[action(namespace = workspace, no_json)]
pub(super) struct OpenTreeFile {
    pub(super) preview: bool,
}

#[derive(Action, Clone, Copy, PartialEq)]
#[action(namespace = workspace, no_json)]
pub(super) enum NavigateTree {
    Up,
    Down,
    Left,
    Right,
}

impl TreeAction {
    fn shortcut_action(self) -> Option<Box<dyn Action>> {
        match self {
            Self::CopyEntry(false) => Some(Box::new(tree_transfer::CopyTreeEntry)),
            Self::CopyEntry(true) => Some(Box::new(tree_transfer::CutTreeEntry)),
            Self::PasteEntry => Some(Box::new(tree_transfer::PasteTreeEntry)),
            Self::Rename => Some(Box::new(RenameTreeEntry)),
            Self::Delete => Some(Box::new(DeleteTreeEntry)),
            Self::Refresh => Some(Box::new(RefreshTree)),
            _ => None,
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Open(true) => IconName::FileInput,
            Self::Open(false) => IconName::Pin,
            Self::OpenInDocs => IconName::BookOpen,
            Self::OpenSource => IconName::FileCode,
            Self::PreviewAudio => IconName::Play,
            Self::SaveCopy => IconName::Download,
            Self::Create(false) => IconName::FilePlus,
            Self::Create(true) => IconName::FolderPlus,
            Self::CopyPath(_) => IconName::Link2,
            Self::CopyEntry(false) => IconName::Copy,
            Self::CopyEntry(true) => IconName::Scissors,
            Self::PasteEntry => IconName::ClipboardPaste,
            Self::Rename => IconName::Pencil,
            Self::Delete => IconName::Trash,
            Self::Refresh => IconName::RefreshCw,
            Self::Collapse => IconName::ChevronsDownUp,
        }
    }
}

pub(super) fn tree_menu(
    element: Stateful<Div>,
    entry: Option<Entry>,
    workspace: WeakEntity<Workspace>,
) -> AnyElement {
    element
        .context_menu(move |mut menu, _, cx| {
            let Some((repo, busy, clipboard, paste, focus)) = workspace
                .read_with(cx, |this, _| {
                    this.repository.clone().map(|repo| {
                        (
                            repo,
                            this.tree_busy || this.tree_edit.is_some(),
                            this.tree_clipboard_available(cx),
                            this.tree_paste_available(cx),
                            this.tree_focus.clone(),
                        )
                    })
                })
                .ok()
                .flatten()
            else {
                return menu;
            };
            menu = menu.action_context(focus);
            let mutable = !busy
                && repo
                    .ssh
                    .as_ref()
                    .is_none_or(|ssh| ssh.supports_workspace_files());
            if !busy && !mutable {
                menu = menu
                    .item(PopupMenuItem::new(t!("workspace.tree_requires_helper")).disabled(true))
                    .separator();
            } else if !busy
                && repo
                    .ssh
                    .as_ref()
                    .is_some_and(|ssh| !ssh.supports_workspace_transfer())
            {
                menu = menu
                    .item(
                        PopupMenuItem::new(t!("workspace.tree_transfer_requires_helper"))
                            .disabled(true),
                    )
                    .separator();
            }
            let item = tree_action_item(&workspace, &repo, &entry, WorkspaceView::Editor);
            if entry.as_ref().is_none_or(|entry| entry.directory) {
                menu = menu
                    .item(item(
                        TreeAction::Create(false),
                        t!("workspace.tree_new_file").into(),
                        mutable,
                    ))
                    .item(item(
                        TreeAction::Create(true),
                        t!("workspace.tree_new_directory").into(),
                        mutable,
                    ))
                    .separator();
            } else if let Some(entry) = &entry
                && !entry.symlink
            {
                if audio_preview::is_audio(&entry.path) {
                    menu = menu.item(item(
                        TreeAction::PreviewAudio,
                        t!("audio.preview").into(),
                        true,
                    ));
                } else {
                    menu = menu
                        .item(item(
                            TreeAction::Open(true),
                            t!("workspace.tree_open").into(),
                            true,
                        ))
                        .item(item(
                            TreeAction::Open(false),
                            t!("workspace.tree_keep_open").into(),
                            true,
                        ));
                    if docs::is_markdown(&entry.path) {
                        menu = menu.item(item(
                            TreeAction::OpenInDocs,
                            t!("tabs.open_in_docs").into(),
                            true,
                        ));
                    }
                }
                menu = menu
                    .item(item(
                        TreeAction::SaveCopy,
                        t!("workspace.tree_save_copy").into(),
                        true,
                    ))
                    .separator();
            }
            if repo.ssh.is_none() {
                menu = system_menu_for_entry(menu, entry.as_ref(), &workspace, cx).separator();
            }
            if let Some(entry) = entry.as_ref() {
                menu = menu
                    .item(item(
                        TreeAction::CopyEntry(false),
                        t!("workspace.tree_copy_entry").into(),
                        clipboard && !entry.symlink,
                    ))
                    .item(item(
                        TreeAction::CopyEntry(true),
                        t!("workspace.tree_cut_entry").into(),
                        clipboard,
                    ));
            }
            if entry.as_ref().is_none_or(|entry| entry.directory) {
                menu = menu.item(item(
                    TreeAction::PasteEntry,
                    t!("workspace.tree_paste_entry").into(),
                    paste,
                ));
            }
            menu = menu.separator();
            if entry.is_some() {
                path_copy_menu(menu, &item)
                    .separator()
                    .item(item(
                        TreeAction::Rename,
                        t!("workspace.tree_rename").into(),
                        mutable,
                    ))
                    .item(item(
                        TreeAction::Delete,
                        t!("workspace.tree_delete").into(),
                        mutable,
                    ))
            } else {
                menu.item(item(
                    TreeAction::Refresh,
                    t!("workspace.tree_refresh").into(),
                    true,
                ))
                .item(item(
                    TreeAction::Collapse,
                    t!("workspace.collapse_all").into(),
                    !busy,
                ))
            }
        })
        .shared_layer()
        .into_any_element()
}

pub(super) fn document_tree_menu(
    element: Stateful<Div>,
    entry: Option<Entry>,
    workspace: WeakEntity<Workspace>,
) -> AnyElement {
    element
        .context_menu(move |menu, _, cx| {
            let Some(repo) = workspace
                .read_with(cx, |this, _| this.repository.clone())
                .ok()
                .flatten()
            else {
                return menu;
            };
            let item = tree_action_item(&workspace, &repo, &entry, WorkspaceView::Docs);
            let menu = menu.when(
                entry
                    .as_ref()
                    .is_some_and(|entry| !entry.directory && !entry.symlink),
                |menu| {
                    menu.item(item(
                        TreeAction::OpenSource,
                        t!("docs.open_source").into(),
                        true,
                    ))
                    .separator()
                },
            );
            let menu = system_menu_for_entry(menu, entry.as_ref(), &workspace, cx);
            if entry.is_some() {
                path_copy_menu(menu.separator(), &item)
            } else {
                menu
            }
        })
        .shared_layer()
        .into_any_element()
}

fn tree_action_item<'a>(
    workspace: &'a WeakEntity<Workspace>,
    repo: &'a Arc<Repository>,
    entry: &'a Option<Entry>,
    view: WorkspaceView,
) -> impl Fn(TreeAction, SharedString, bool) -> PopupMenuItem + 'a {
    move |action, label, enabled| {
        let workspace = workspace.clone();
        let repo = repo.clone();
        let entry = entry.clone();
        PopupMenuItem::new(label)
            .icon(action.icon())
            .when_some(action.shortcut_action(), |item, action| item.action(action))
            .disabled(!enabled)
            .on_click(move |_, window, cx| {
                _ = workspace.update(cx, |this, cx| {
                    if this.view == view
                        && this
                            .repository
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &repo))
                    {
                        this.tree_action(action, entry.clone(), window, cx);
                    }
                });
            })
    }
}

fn path_copy_menu(
    menu: PopupMenu,
    item: &impl Fn(TreeAction, SharedString, bool) -> PopupMenuItem,
) -> PopupMenu {
    menu.item(item(
        TreeAction::CopyPath(false),
        t!("tabs.copy_relative_path").into(),
        true,
    ))
    .item(item(
        TreeAction::CopyPath(true),
        t!("tabs.copy_absolute_path").into(),
        true,
    ))
}

fn system_menu_for_entry(
    menu: PopupMenu,
    entry: Option<&Entry>,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> PopupMenu {
    if let Some(workspace) = workspace.upgrade() {
        let path = entry.map_or(Path::new(""), |entry| &entry.path);
        let directory = entry.is_none_or(|entry| entry.directory);
        workspace
            .read(cx)
            .system_tree_menu(menu, path, directory, false, false)
    } else {
        menu
    }
}

impl Workspace {
    pub(super) fn scroll_tree_with_wheel(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.content_delta().x;
        if event.is_wheel() && !delta.is_zero() {
            self.scroll_tree_horizontally(delta * HORIZONTAL_WHEEL_SCALE, window, cx);
            cx.stop_propagation();
        }
    }

    pub(super) fn scroll_tree_with_side_button(
        &self,
        direction: NavigationDirection,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let visible = match self.view {
            WorkspaceView::Editor => self.session.panels.editor_tree,
            WorkspaceView::Docs => self.session.panels.docs_tree,
            WorkspaceView::Git => false,
        };
        let handle = &self.tree_horizontal.handle;
        if !self.config.current().appearance.file_tree_horizontal_scroll
            || !visible
            || self.standalone.is_some()
            || self.tree_edit.is_some()
            || self.confirming_close
            || self.repository_overlay_open()
            || self.repository_ui.opening.is_some()
            || self.locator.open
            || window.has_active_dialog(cx)
            || window.has_active_prompt()
            || handle.max_offset().x <= px(0.)
            || !handle.bounds().contains(&position)
        {
            return false;
        }

        let step = match direction {
            NavigationDirection::Back => SIDE_BUTTON_SCROLL_STEP,
            NavigationDirection::Forward => -SIDE_BUTTON_SCROLL_STEP,
        };
        self.scroll_tree_horizontally(step, window, cx);
        true
    }

    fn scroll_tree_horizontally(&self, step: Pixels, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.tree_horizontal.handle.clone();
        let smooth_scroll = handle.smooth_scroll();
        let current = handle.offset();
        let target = smooth_scroll.target(current, point(step, px(0.)), window);
        let target = point(target.x.clamp(-handle.max_offset().x, px(0.)), current.y);
        let view = cx.entity_id();
        smooth_scroll.animate(current, target, window, cx, move |last, next, _, cx| {
            if handle.offset() != last {
                return None;
            }
            let next = point(next.x.clamp(-handle.max_offset().x, px(0.)), next.y);
            if next != last {
                handle.set_offset(next);
                cx.notify(view);
            }
            Some(next)
        });
    }

    pub(super) fn reveal_tree_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        let expanded = if self.view == WorkspaceView::Docs {
            &mut self.docs_expanded
        } else {
            &mut self.expanded
        };
        let count = expanded.len();
        expanded.extend(
            path.ancestors()
                .skip(1)
                .take_while(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_owned),
        );
        if expanded.len() != count {
            self.rebuild_tree();
        }
        if let Some(index) = self.rows.iter().position(|row| row.entry.path == path) {
            self.scroll_tree_row_into_view(index);
            cx.notify();
        }
    }

    fn scroll_tree_row_into_view(&self, index: usize) {
        let inset = if self.config.current().appearance.file_tree_sticky_scroll {
            let viewport = self.tree_scroll.0.borrow().base_handle.bounds();
            tree_sticky_scroll::row_limit(viewport.size.height).min(self.rows[index].depth)
        } else {
            0
        };
        self.tree_scroll
            .scroll_to_item_with_offset(index, ScrollStrategy::Nearest, inset);
    }

    pub(super) fn open_tree_file(
        &mut self,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tree_focus.is_focused(window)
            || self.tree_edit.is_some()
            || self.repository_ui.opening.is_some()
            || self.confirming_close
        {
            return;
        }
        let Some(entry) = self.rows.iter().map(|row| &row.entry).find(|entry| {
            entry.path == self.tree_keyboard_path && !entry.directory && !entry.symlink
        }) else {
            return;
        };
        let path = entry.path.clone();
        if self.view == WorkspaceView::Docs {
            self.navigate_document(path, None, window, cx);
        } else if audio_preview::is_audio(&path) {
            self.preview_audio(path, window, cx);
        } else {
            self.request_file(path, FileOpen::Tree { preview }, window, cx);
        }
    }

    pub(super) fn navigate_tree(
        &mut self,
        direction: NavigateTree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tree_focus.is_focused(window) || self.tree_edit.is_some() || self.rows.is_empty() {
            return;
        }

        let mut visible_path = self
            .tree_keyboard_path
            .ancestors()
            .filter_map(|path| self.rows.iter().position(|row| row.entry.path == path));
        let target = match visible_path.next() {
            Some(index) if self.rows[index].entry.path == self.tree_keyboard_path => {
                let row = &self.rows[index];
                match direction {
                    NavigateTree::Up => index.checked_sub(1),
                    NavigateTree::Down => (index + 1 < self.rows.len()).then_some(index + 1),
                    NavigateTree::Left if row.expanded => {
                        self.toggle_tree_directory(row.entry.path.clone(), window, cx);
                        Some(index)
                    }
                    NavigateTree::Left => row.parent,
                    NavigateTree::Right if row.entry.directory && !row.expanded => {
                        self.toggle_tree_directory(row.entry.path.clone(), window, cx);
                        Some(index)
                    }
                    NavigateTree::Right => (index + 1 < row.subtree_end).then_some(index + 1),
                }
            }
            ancestor => Some(ancestor.unwrap_or(0)),
        };
        if let Some(index) = target {
            self.scroll_tree_row_into_view(index);
            self.focus_tree(self.rows[index].entry.path.clone(), window, cx);
        }
    }

    pub(super) fn tree_key_action(
        &mut self,
        action: TreeAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view != WorkspaceView::Editor || self.tree_busy || self.tree_edit.is_some() {
            return;
        }
        let Some(repo) = &self.repository else {
            return;
        };
        if !matches!(action, TreeAction::Refresh)
            && repo
                .ssh
                .as_ref()
                .is_some_and(|ssh| !ssh.supports_workspace_files())
        {
            return;
        }
        let entry = self
            .rows
            .iter()
            .find(|row| row.entry.path == self.tree_keyboard_path)
            .map(|row| row.entry.clone());
        if entry.is_some() || matches!(action, TreeAction::Refresh) {
            self.tree_action(action, entry, window, cx);
        }
    }

    fn tree_action(
        &mut self,
        action: TreeAction,
        entry: Option<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let path = entry
            .as_ref()
            .map(|entry| entry.path.clone())
            .unwrap_or_default();
        match action {
            TreeAction::Open(preview) => self.open_file(path, preview, window, cx),
            TreeAction::OpenInDocs => self.open_markdown_in_docs(path, window, cx),
            TreeAction::OpenSource => self.open_document_source(path, window, cx),
            TreeAction::PreviewAudio => self.preview_audio(path, window, cx),
            TreeAction::SaveCopy => self.save_tree_copy(repo, path, window, cx),
            TreeAction::CopyPath(absolute) => {
                let path = if absolute { repo.root.join(path) } else { path };
                cx.write_to_clipboard(ClipboardItem::new_string(paths::display(&path)));
            }
            TreeAction::CopyEntry(cut) => {
                if let Some(entry) = entry.as_ref() {
                    self.tree_copy(entry, cut, window, cx);
                }
            }
            TreeAction::PasteEntry => self.tree_paste(path, false, window, cx),
            TreeAction::Create(directory) => {
                self.begin_tree_edit(path, directory, None, window, cx)
            }
            TreeAction::Rename | TreeAction::Delete => {
                if self.tree_busy || self.tree_edit.is_some() {
                    return;
                }
                self.tree_busy = true;
                let lookup = repo.clone();
                let source = path.clone();
                let version = cx.background_spawn(async move { lookup.entry_version(&source) });
                cx.spawn_in(window, async move |this, cx| {
                    let version = version.await;
                    _ = this.update_in(cx, |this, window, cx| {
                        if !this
                            .repository
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &repo))
                        {
                            return;
                        }
                        this.tree_busy = false;
                        cx.notify();
                        if this.view != WorkspaceView::Editor {
                            return;
                        }
                        match version {
                            Ok(version) if matches!(action, TreeAction::Rename) => {
                                this.begin_tree_edit(
                                    path,
                                    entry.unwrap().directory,
                                    Some(version),
                                    window,
                                    cx,
                                );
                            }
                            Ok(version) => this.confirm_tree_delete(path, version, window, cx),
                            Err(error) => this.show_error(error, window, cx),
                        }
                    });
                })
                .detach();
            }
            TreeAction::Refresh => self.rescan_workspace(window, cx),
            TreeAction::Collapse => {
                self.tree_motion.get_mut().start();
                self.expanded.clear();
                self.tree_reveal = None;
                self.rebuild_tree();
                cx.notify();
            }
        }
    }

    fn begin_tree_edit(
        &mut self,
        path: PathBuf,
        directory: bool,
        version: Option<EntryVersion>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree_busy || self.tree_edit.is_some() {
            return;
        }
        let value = if version.is_some() {
            file_name(&path)
        } else {
            ""
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(value.to_owned()));
        let return_focus = window.focused(cx);
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit_tree_edit(window, cx);
            }
        });
        if version.is_none() && !path.as_os_str().is_empty() {
            self.expanded.insert(path.clone());
            self.load_expanded_tree_directories(window, cx);
            self.rebuild_tree();
        }
        let edit = TreeEdit {
            path,
            directory,
            version,
            input,
            return_focus,
            _subscription: subscription,
        };
        if let Some((index, _, _)) = edit.row(&self.rows) {
            self.tree_scroll.scroll_to_item(index, ScrollStrategy::Top);
        }
        edit.input.read(cx).focus_handle(cx).focus(window, cx);
        edit.input
            .update(cx, |state, cx| state.select_all(window, cx));
        self.tree_edit = Some(edit);
        cx.notify();
    }

    pub(super) fn finish_tree_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(edit) = self.tree_edit.take()
            && let Some(focus) = edit.return_focus
        {
            focus.focus(window, cx);
        }
        self.resume_tree_reveal(window, cx);
        cx.notify();
    }

    fn submit_tree_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.tree_edit.as_ref() else {
            return;
        };
        let name = edit.input.read(cx).value().to_string();
        if !valid_name(&name) {
            toast::error(window, cx, t!("workspace.tree_invalid_name"));
            return;
        }
        let (path, operation) = match &edit.version {
            Some(version) => (
                edit.path.clone(),
                FileOperation::Rename {
                    name,
                    version: version.clone(),
                },
            ),
            None => (
                edit.path.join(name),
                FileOperation::Create {
                    directory: edit.directory,
                },
            ),
        };
        self.run_tree_operation(path, operation, window, cx);
    }

    fn affected_tree_files(&self, path: &Path) -> BTreeSet<PathBuf> {
        self.repository_snapshot
            .as_ref()
            .or(self.repository.as_ref())
            .into_iter()
            .flat_map(|repo| repo.children.values().flatten())
            .filter(|entry| !entry.directory && entry.path.starts_with(path))
            .map(|entry| entry.path.clone())
            .chain(
                self.tabs
                    .paths()
                    .iter()
                    .filter(|item| item.starts_with(path))
                    .cloned(),
            )
            .chain(
                self.files
                    .keys()
                    .filter(|item| item.starts_with(path))
                    .cloned(),
            )
            .chain(
                self.session
                    .files
                    .keys()
                    .filter(|item| item.starts_with(path))
                    .cloned(),
            )
            .collect()
    }

    fn confirm_tree_delete(
        &self,
        path: PathBuf,
        version: EntryVersion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.weak_entity();
        let repo = self.repository.clone().unwrap();
        let drafts: Vec<_> = self
            .affected_tree_files(&path)
            .into_iter()
            .filter(|path| self.tab_is_dirty(path))
            .map(|path| paths::display(&path))
            .collect();
        window.open_dialog(cx, move |dialog, _, _| {
            let workspace = workspace.clone();
            let path = path.clone();
            let version = version.clone();
            let repo = repo.clone();
            dialog
                .centered()
                .width(px(480.))
                .title(t!("workspace.tree_delete_title"))
                .child(div().child(paths::display(&path)))
                .when(!drafts.is_empty(), |dialog| {
                    dialog.child(
                        div()
                            .text_sm()
                            .child(t!("workspace.tree_delete_drafts"))
                            .children(
                                drafts
                                    .iter()
                                    .take(6)
                                    .cloned()
                                    .map(|path| div().truncate().child(path)),
                            )
                            .when(drafts.len() > 6, |list| {
                                list.child(t!(
                                    "workspace.tree_more_drafts",
                                    count = drafts.len() - 6
                                ))
                            }),
                    )
                })
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            action_button("cancel-file-delete")
                                .secondary()
                                .label(t!("common.cancel"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            action_button("confirm-file-delete")
                                .danger()
                                .label(t!("workspace.tree_delete"))
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    _ = workspace.update(cx, |this, cx| {
                                        if this
                                            .repository
                                            .as_ref()
                                            .is_some_and(|current| Arc::ptr_eq(current, &repo))
                                        {
                                            this.run_tree_operation(
                                                path.clone(),
                                                FileOperation::Delete {
                                                    version: version.clone(),
                                                },
                                                window,
                                                cx,
                                            );
                                        }
                                    });
                                }),
                        ),
                )
        });
    }

    fn run_tree_operation(
        &mut self,
        path: PathBuf,
        operation: FileOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree_busy {
            return;
        }
        if self
            .saving
            .iter()
            .chain(self.git_edit.iter())
            .any(|item| item.starts_with(&path))
        {
            toast::error(window, cx, t!("workspace.tree_operation_busy"));
            return;
        }
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let action = match &operation {
            FileOperation::Rename { name, .. } => Some(Some(path.with_file_name(name))),
            FileOperation::Delete { .. } => Some(None),
            FileOperation::Create { .. } => None,
            FileOperation::Transfer { .. } => {
                unreachable!("Transfers use the shared tree transfer flow")
            }
        };
        let open = matches!(operation, FileOperation::Create { directory: false });
        let affected = action
            .as_ref()
            .map(|_| self.affected_tree_files(&path))
            .unwrap_or_default();
        self.cancel_file_open();
        self.tree_busy = true;
        let target = path.clone();
        let working = repo.clone();
        let task = cx.background_spawn(async move { working.operate(&target, operation) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |this, window, cx| {
                if !this
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                {
                    return;
                }
                this.tree_busy = false;
                match result {
                    Ok(()) => {
                        this.finish_tree_edit(window, cx);
                        if let Some(action) = action {
                            if let Some(destination) = action {
                                this.relocate_tree_path(
                                    &repo,
                                    &path,
                                    &destination,
                                    affected,
                                    window,
                                    cx,
                                );
                            } else {
                                this.remove_tree_path(&repo, &path, affected, window, cx);
                            }
                        }
                        this.refresh_tree_paths(
                            [path.parent().unwrap_or(Path::new("")).to_owned()],
                            window,
                            cx,
                        );
                        if open && this.view == WorkspaceView::Editor {
                            this.open_file(path, false, window, cx);
                        }
                    }
                    Err(error) => {
                        this.show_error(error, window, cx);
                        this.rescan_workspace(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn relocate_tree_path(
        &mut self,
        repo: &Repository,
        from: &Path,
        to: &Path,
        paths: BTreeSet<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if from == to {
            return;
        }
        let map = |path: &Path| {
            path.strip_prefix(from).map_or_else(
                |_| path.to_owned(),
                |suffix| {
                    if suffix.as_os_str().is_empty() {
                        to.to_owned()
                    } else {
                        to.join(suffix)
                    }
                },
            )
        };
        let active_moved = self
            .tabs
            .active()
            .is_some_and(|path| path.starts_with(from));
        self.capture_active_file_position(cx);
        self.capture_document_position(cx);
        let mut relocations = Vec::new();
        for path in paths {
            let target = map(&path);
            if let Some(mut file) = self.files.remove(&path) {
                if self.files.contains_key(&target) {
                    self.files.insert(path, file);
                    continue;
                }
                let state = file.draft_write.clone();
                let mut pending = state.lock().unwrap();
                let draft = (file.committed != file.saved).then(|| file.committed.to_string());
                relocations.push(self.store.draft_relocation(
                    repo,
                    &path,
                    &target,
                    Some((file.document.draft_baseline(), draft)),
                ));
                pending.revision = file.revision;
                pending.result = None;
                file.path = target.clone();
                repo.language_resources().configure_editor(&target, cx);
                let old_syntax = self.associated_highlight(&path).map(|(_, syntax)| syntax);
                let new_syntax = self.associated_highlight(&target).map(|(_, syntax)| syntax);
                let syntax_changed = old_syntax != new_syntax;
                file.editor.update(cx, |editor, cx| {
                    let language = self.source_language(&target, editor.text());
                    let language_changed = editor.language_name() != language;
                    if language_changed {
                        editor.set_highlighter(language, cx);
                    }
                    if syntax_changed {
                        editor.set_highlighter_factory(source_highlighter_factory(new_syntax), cx);
                        editor.apply_highlighter_fold_candidates(Vec::new(), cx);
                    }
                    if syntax_changed || language_changed {
                        editor.prepare_language(window, cx);
                    }
                });
                file.language_server = None;
                file.server_project = ServerProject::for_file(repo, &target);
                self.files.insert(target.clone(), file);
                self.connect_file_to_language_service(&target, cx);
            } else {
                relocations.push(self.store.draft_relocation(repo, &path, &target, None));
            }
            if let Some(position) = self.session.files.remove(&path) {
                self.session.files.insert(target, position);
            }
        }
        let images: Vec<_> = self
            .images
            .keys()
            .filter(|path| path.starts_with(from))
            .cloned()
            .collect();
        for path in images {
            let target = map(&path);
            if crate::image_viewer::is_image(&target) && !self.images.contains_key(&target) {
                let image = self.images.remove(&path).unwrap();
                self.images.insert(target.clone(), image);
                self.load_image_tab(target, window, cx);
            } else {
                self.remove_image_tab(&path, window);
            }
        }
        self.tabs.remap(&map);
        if let Some(history) = self.source_history_mut() {
            history.remap(&map);
        }
        self.tree_keyboard_path = map(&self.tree_keyboard_path);
        self.expanded = self.expanded.iter().map(|path| map(path)).collect();
        self.docs_expanded = self.docs_expanded.iter().map(|path| map(path)).collect();
        self.session.documents = std::mem::take(&mut self.session.documents)
            .into_iter()
            .map(|(path, position)| (map(&path), position))
            .collect();
        if let Some(docs) = self.session.open_documents.as_mut() {
            for doc in docs {
                doc.path = map(&doc.path);
            }
        }
        if let Some(doc) = self
            .session
            .document
            .clone()
            .filter(|path| path.starts_with(from))
        {
            let target = map(&doc);
            self.session.document = Some(target.clone());
            self.doc = None;
            if self.view == WorkspaceView::Docs {
                self.open_doc(target, None, window, cx);
            }
        }
        if active_moved && self.view == WorkspaceView::Editor {
            if self.active_file().is_none() && self.active_image_tab().is_none() {
                self.restore_active_file(window, cx);
            } else {
                self.watch_editor_git(window, cx);
            }
        }
        self.session_revision += 1;
        self.queue_draft_relocations(relocations);
        if let Err(error) = self.flush_session(cx) {
            self.show_error(error, window, cx);
        }
        self.rebuild_tree();
    }

    pub(super) fn files_for_external_rename(&self, source: &Path) -> BTreeSet<PathBuf> {
        self.affected_tree_files(source)
    }

    fn remove_tree_path(
        &mut self,
        repo: &Repository,
        target: &Path,
        paths: BTreeSet<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree_keyboard_path.starts_with(target) {
            self.tree_keyboard_path = target.parent().unwrap_or(Path::new("")).to_owned();
        }
        let previous = self.tabs.active().map(Path::to_owned);
        if matches!(
            self.loading_file,
            Some((_, super::FileOpen::History { .. }))
        ) {
            self.cancel_file_open();
        }
        if let Some(history) = self.source_history_mut() {
            history.remove(target);
        }
        for path in paths {
            if let Some(file) = self.files.remove(&path) {
                let mut pending = file.draft_write.lock().unwrap();
                pending.revision = file.revision;
            }
            self.remove_image_tab(&path, window);
            self.tabs.close(&path);
            self.session.files.remove(&path);
            if let Err(error) = self.store.discard_draft(repo, &path) {
                self.show_error(error, window, cx);
            }
        }
        let removed: Vec<_> = self
            .session
            .open_documents
            .iter()
            .flatten()
            .filter(|document| document.path.starts_with(target))
            .map(|document| document.path.clone())
            .collect();
        for path in removed {
            self.session.close_document(&path);
            self.session.documents.remove(&path);
        }
        if self
            .doc
            .as_ref()
            .is_some_and(|doc| doc.read(cx).path.starts_with(target))
        {
            self.doc = None;
            if self.view == WorkspaceView::Docs
                && let Some(path) = self.session.document.clone()
            {
                self.open_doc(path, None, window, cx);
            }
        }
        self.expanded.retain(|path| !path.starts_with(target));
        self.docs_expanded.retain(|path| !path.starts_with(target));
        if previous.as_deref() != self.tabs.active() {
            self.restore_active_file(window, cx);
        }
        self.session_revision += 1;
    }

    fn save_tree_copy(
        &self,
        repo: Arc<Repository>,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let buffer = self.files.get(&path).and_then(|file| {
            let text = file.editor.read(cx).text();
            (text != &file.saved).then(|| file.document.encode_text(&text.to_string()))
        });
        let store = self.store.clone();
        let initial = if repo.ssh.is_none() {
            repo.root.join(path.parent().unwrap_or(Path::new("")))
        } else {
            dirs::home_dir().unwrap_or_else(env::temp_dir)
        };
        let selection =
            cx.prompt_for_new_path(&initial, path.file_name().and_then(|name| name.to_str()));
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let Some(destination) = selection.await?? else {
                    return Ok(());
                };
                let source = repo.ssh.is_none().then(|| repo.root.join(&path));
                let target = cx
                    .background_spawn(
                        async move { CopyTarget::prepare(destination, source.as_deref()) },
                    )
                    .await?;
                if target.exists() {
                    let confirmation = this.update_in(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Warning,
                            &t!("workspace.tree_replace_title"),
                            Some(&paths::display(target.path())),
                            &[
                                PromptButton::new(t!("common.cancel")),
                                PromptButton::new(t!("workspace.tree_replace")),
                            ],
                            cx,
                        )
                    })?;
                    if confirmation.await? != 1 {
                        return Ok(());
                    }
                }
                cx.background_spawn(async move {
                    target.write(|output| {
                        if let Some(bytes) = buffer {
                            output.write_all(&bytes)?;
                            return Ok(());
                        }
                        if store.has_draft(&repo, &path) {
                            let document = Document::open(&repo, &path, &store)?;
                            if document.restored {
                                output.write_all(&document.encode_text(&document.text))?;
                                return Ok(());
                            }
                        }
                        repo.copy_to(&path, output)
                    })
                })
                .await
            }
            .await;
            if let Err(error) = result {
                _ = this.update_in(cx, |this, window, cx| this.show_error(error, window, cx));
            }
        })
        .detach();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DirectoryStatus {
    Added,
    Changed,
    Conflict,
}

#[derive(Default, PartialEq)]
pub(super) struct TreeStatus {
    files: Arc<crate::git::WorktreeStatus>,
    directories: HashMap<PathBuf, DirectoryStatus>,
}

impl TreeStatus {
    pub(super) fn new(files: Arc<crate::git::WorktreeStatus>) -> Self {
        let mut directories = HashMap::<_, DirectoryStatus>::new();
        for (path, status) in files.iter() {
            let change = match status.code {
                _ if status.conflict => DirectoryStatus::Conflict,
                'A' | '?' => DirectoryStatus::Added,
                _ => DirectoryStatus::Changed,
            };
            for path in std::iter::once(path).chain(status.old_path.iter()) {
                for directory in Path::new(path).ancestors().skip(1) {
                    let entry = directories.entry(directory.to_owned()).or_insert(change);
                    *entry = (*entry).max(change);
                }
            }
        }
        Self { files, directories }
    }

    pub(super) fn decoration_color(
        &self,
        entry: &Entry,
        colors: Palette,
        cx: &App,
    ) -> Option<Hsla> {
        let code = if entry.directory {
            match self.directories.get(&entry.path)? {
                DirectoryStatus::Added => 'A',
                DirectoryStatus::Changed => 'M',
                DirectoryStatus::Conflict => 'U',
            }
        } else {
            let key = entry.path.to_str()?.replace(std::path::MAIN_SEPARATOR, "/");
            let status = self.files.get(&key)?;
            if status.conflict { 'U' } else { status.code }
        };
        let mut color = super::git_panel::status_color(code, colors, cx);
        if matches!(code, 'A' | '?') {
            color.s *= 0.55;
        }
        Some(color)
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn tree_directories_use_added_color_when_all_changes_are_additions() {
        use crate::git::WorktreeFile;

        let files = [
            ("new/staged.txt", 'A', None, false),
            ("new/nested/untracked.txt", '?', None, false),
            ("mixed/added.txt", 'A', None, false),
            ("mixed/modified.txt", 'M', None, false),
            ("conflicted/added.txt", 'A', None, false),
            ("conflicted/nested/conflict.txt", 'U', None, true),
            (
                "moved-to/renamed.txt",
                'R',
                Some("moved-from/renamed.txt"),
                false,
            ),
            ("moved-from/added.txt", '?', None, false),
        ]
        .into_iter()
        .map(|(path, code, old_path, conflict)| {
            let mut status = WorktreeFile::default();
            status.code = code;
            status.old_path = old_path.map(str::to_owned);
            status.conflict = conflict;
            (path.to_owned(), status)
        })
        .collect();
        let status = TreeStatus::new(Arc::new(files));

        TestAppContext::single().update(|cx| {
            gpui_component::init(cx);
            let colors = Palette::for_mode(cx.theme().mode);
            let mut added = cx.theme().success;
            added.s *= 0.55;
            for (path, color) in [
                ("new", Some(added)),
                ("new/nested", Some(added)),
                ("mixed", Some(colors.warning)),
                ("conflicted", Some(cx.theme().danger)),
                ("moved-from", Some(colors.warning)),
                ("moved-to", Some(colors.warning)),
                ("clean", None),
            ] {
                let directory = Entry {
                    path: path.into(),
                    directory: true,
                    symlink: false,
                    ignored: false,
                };
                assert_eq!(
                    status.decoration_color(&directory, colors, cx),
                    color,
                    "{path}"
                );
            }
        });
    }

    fn test_repository_location(root: PathBuf) -> PathBuf {
        match env::var("STROLL_TEST_WORKSPACE_SSH_AUTHORITY") {
            Ok(authority) => PathBuf::from(format!("ssh://{authority}{}", root.display())),
            Err(_) => root,
        }
    }

    #[track_caller]
    fn settle(
        visual: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
        ready: impl Fn(&Workspace) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            visual.executor().advance_clock(crate::git::watch::DEBOUNCE);
            visual.run_until_parked();
            let done = visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
                ready(workspace.read(cx))
            });
            if done {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "workspace did not settle"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[::core::prelude::v1::test]
    fn document_tree_keyboard_navigation_and_open_reveal() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir_all(root.join("guide/nested"))?;
        fs::write(root.join("README.md"), "# Project\n")?;
        for index in 0..40 {
            fs::write(root.join(format!("guide/nested/{index:02}.md")), "# Page\n")?;
        }
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q", "--"])
                .arg(&root)
                .status()?
                .success()
        );

        let mut cx = TestAppContext::single();
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut workspace = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let view =
                cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Docs, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        let visual = &mut visual;
        visual.simulate_resize(size(px(900.), px(500.)));
        visual.update(|window, cx| {
            window.activate_window();
            cx.set_reduce_motion(true);
            workspace.update(cx, |this, cx| {
                this.open_repository(Location { root, file: None }, window, cx);
            });
        });

        let document_is_open = |this: &Workspace, path: &str| {
            this.doc.is_some()
                && this.doc_loading.is_none()
                && this.session.document.as_deref() == Some(Path::new(path))
        };
        let assert_visible = |visual: &mut VisualTestContext, selector: &'static str| {
            let row = visual
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing {selector}"));
            let viewport = visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .tree_scroll
                    .0
                    .borrow()
                    .base_handle
                    .bounds()
            });
            assert!(row.top() >= viewport.top() && row.bottom() <= viewport.bottom());
        };

        settle(visual, &workspace, |this| {
            this.docs_tree_complete && document_is_open(this, "README.md")
        });
        let guide = visual.debug_bounds("workspace-tree-row:guide").unwrap();
        visual.simulate_click(guide.center(), Modifiers::default());
        visual.simulate_keystrokes("right right right");
        visual.simulate_keystrokes(&vec!["down"; 39].join(" "));
        settle(visual, &workspace, |_| true);
        let target = "guide/nested/39.md";
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert!(this.tree_focus.is_focused(window));
            assert_eq!(this.tree_keyboard_path, Path::new(target));
            assert!(document_is_open(this, "README.md"));
        });
        assert_visible(visual, "workspace-tree-row:guide/nested/39.md");

        visual.simulate_keystrokes("enter");
        settle(visual, &workspace, |this| document_is_open(this, target));
        visual.simulate_keystrokes("up space");
        settle(visual, &workspace, |this| {
            document_is_open(this, "guide/nested/38.md")
        });

        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.focus.focus(window, cx);
                this.set_file_tree_visible(false);
                this.toggle_tree_directory("guide".into(), window, cx);
                this.docs_tree = Rc::new(DocumentTree::from_paths(vec!["README.md".into()]));
                this.docs_tree_complete = false;
                this.docs_tree_dirty = true;
                this.rebuild_tree();
                this.follow_document_link(Path::new("README.md"), target, window, cx);
            });
        });
        settle(visual, &workspace, |this| document_is_open(this, target));
        let show = visual.debug_bounds("show-file-tree").unwrap();
        visual.simulate_click(show.center(), Modifiers::default());
        settle(visual, &workspace, |_| true);
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert_eq!(this.docs_tree_reveal.as_deref(), Some(Path::new(target)));
                this.load_docs_tree(window, cx);
            });
        });
        settle(visual, &workspace, |this| {
            this.docs_tree_complete && this.docs_tree_reveal.is_none()
        });
        assert_visible(visual, "workspace-tree-row:guide/nested/39.md");
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert!(!this.tree_focus.is_focused(window));
            assert!(this.docs_expanded.contains(Path::new("guide")));
            assert!(this.docs_expanded.contains(Path::new("guide/nested")));
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn workspace_and_docs_tree_horizontal_scroll() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir_all(root.join("a"))?;
        for index in 0..26 {
            fs::write(root.join(format!("a/{index:02}.md")), "# Page\n")?;
        }
        let long_path = format!("a/zz-{}.md", "long-file-name-".repeat(6));
        fs::write(root.join(&long_path), "# Long title\n")?;
        fs::write(root.join("README.md"), "# Project\n")?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-qb", "main"])
                .arg(&root)
                .status()?
                .success()
        );

        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config_path = fixture.path().join("config.toml");
        let config = ConfigFile::load(Some(config_path.clone()));
        assert!(!config.current().appearance.file_tree_horizontal_scroll);
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(900.), px(560.)));
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                this.open_repository(Location { root, file: None }, window, cx);
            });
        });

        let configure = |visual: &mut VisualTestContext, enabled| {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    let previous = this.config.clone();
                    let mut config = previous.current().clone();
                    config.appearance.file_tree_horizontal_scroll = enabled;
                    this.config.save(config).unwrap();
                    this.apply_config(&previous, window, cx);
                    cx.notify();
                });
            });
            settle(visual, &workspace, |_| true);
        };
        let offsets = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                let this = workspace.read(cx);
                point(
                    this.tree_horizontal.handle.offset().x,
                    this.tree_scroll.0.borrow().base_handle.offset().y,
                )
            })
        };
        let click_side = |visual: &mut VisualTestContext, position, direction| {
            let button = MouseButton::Navigate(direction);
            visual.simulate_mouse_down(position, button, Modifiers::none());
            visual.simulate_mouse_up(position, button, Modifiers::none());
        };
        let scroll = |visual: &mut VisualTestContext, position, delta, source| {
            visual.simulate_event(ScrollEvent {
                position,
                delta,
                modifiers: Modifiers::none(),
                touch_phase: TouchPhase::Moved,
                source,
            });
            settle(visual, &workspace, |_| true);
        };

        for view in [WorkspaceView::Editor, WorkspaceView::Docs] {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| this.switch_view(view, window, cx));
            });
            settle(visual, &workspace, |this| {
                (view != WorkspaceView::Docs || this.docs_tree_complete)
                    && this.rows.iter().any(|row| row.entry.path == Path::new("a"))
            });
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.toggle_tree_directory("a".into(), window, cx)
                });
            });
            settle(visual, &workspace, |this| {
                this.rows
                    .iter()
                    .any(|row| row.entry.path == Path::new(&long_path))
            });

            let row_id = "workspace-tree-row:a/00.md";
            let initial = visual.debug_bounds(row_id).unwrap();
            let pointer = point(initial.left() + px(80.), initial.center().y);
            click_side(visual, pointer, NavigationDirection::Forward);
            assert_eq!(offsets(visual).x, px(0.));
            assert_eq!(visual.debug_bounds(row_id), Some(initial));

            configure(visual, true);
            let max =
                visual.update(|_, cx| workspace.read(cx).tree_horizontal.handle.max_offset().x);
            assert!(max > SIDE_BUTTON_SCROLL_STEP * 2.);
            let before = visual.debug_bounds(row_id).unwrap();
            assert!(before.size.width > initial.size.width);

            visual.update(|_, cx| cx.set_smooth_scrolling(false));
            click_side(visual, pointer, NavigationDirection::Forward);
            assert_eq!(offsets(visual), point(-SIDE_BUTTON_SCROLL_STEP, px(0.)));
            click_side(visual, pointer, NavigationDirection::Back);
            assert_eq!(offsets(visual), point(px(0.), px(0.)));

            visual.update(|_, cx| cx.set_smooth_scrolling(true));
            click_side(visual, pointer, NavigationDirection::Forward);
            let during = offsets(visual).x;
            assert!(during > -SIDE_BUTTON_SCROLL_STEP && during <= px(0.));
            for _ in 0..40 {
                visual.executor().advance_clock(Duration::from_millis(16));
                visual.update(|window, cx| window.simulate_next_frame(cx));
            }
            settle(visual, &workspace, |_| true);
            assert_eq!(offsets(visual), point(-SIDE_BUTTON_SCROLL_STEP, px(0.)));
            let after = visual.debug_bounds(row_id).unwrap();
            assert_eq!(after.left(), before.left() - SIDE_BUTTON_SCROLL_STEP);
            assert_eq!(after.top(), before.top());

            scroll(
                visual,
                pointer,
                ScrollDelta::Pixels(point(px(0.), -6. * tree_motion::ROW_HEIGHT)),
                ScrollSource::Pointer,
            );
            let pinned_id = "workspace-tree-sticky-row:a";
            let pinned = visual.debug_bounds(pinned_id).unwrap();
            let y = offsets(visual).y;
            assert!(y < px(0.));
            let pinned_pointer = point(pointer.x, pinned.center().y);
            visual.update(|_, cx| cx.set_smooth_scrolling(false));
            click_side(visual, pinned_pointer, NavigationDirection::Back);
            settle(visual, &workspace, |_| true);
            assert_eq!(offsets(visual), point(px(0.), y));
            scroll(
                visual,
                pinned_pointer,
                ScrollDelta::Lines(point(-3., 0.)),
                ScrollSource::Pointer,
            );
            assert_eq!(offsets(visual), point(px(-15.), y));
            scroll(
                visual,
                pinned_pointer,
                ScrollDelta::Pixels(point(px(-84.), px(0.))),
                ScrollSource::PreciseWheel,
            );
            assert_eq!(offsets(visual), point(px(-36.), y));
            scroll(
                visual,
                pinned_pointer,
                ScrollDelta::Pixels(point(px(12.), px(0.))),
                ScrollSource::Pointer,
            );
            assert_eq!(offsets(visual), point(-SIDE_BUTTON_SCROLL_STEP, y));
            assert_eq!(
                visual.debug_bounds(pinned_id).unwrap().left(),
                pinned.left()
            );

            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.toggle_tree_directory("a".into(), window, cx)
                });
            });
            settle(visual, &workspace, |this| {
                this.tree_horizontal.handle.max_offset().x == px(0.)
            });
            assert_eq!(offsets(visual).x, px(0.));
            assert!(
                ConfigFile::load(Some(config_path.clone()))
                    .current()
                    .appearance
                    .file_tree_horizontal_scroll
            );
            configure(visual, false);
        }
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn workspace_and_docs_tree_sticky_scroll() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        for directory in ["a/b", "a/c"] {
            fs::create_dir_all(root.join(directory))?;
            for index in 0..30 {
                fs::write(
                    root.join(directory).join(format!("{index:02}.md")),
                    "# Page\n",
                )?;
            }
        }
        fs::write(root.join("README.md"), "# Project\n")?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-qb", "main"])
                .arg(&root)
                .status()?
                .success()
        );
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config_path = fixture.path().join("config.toml");
        let config = ConfigFile::load(Some(config_path.clone()));
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(900.), px(560.)));
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                this.open_repository(Location { root, file: None }, window, cx);
            });
        });

        let scroll = |visual: &mut VisualTestContext, position, rows: f32| {
            visual.simulate_event(ScrollEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.), -tree_motion::ROW_HEIGHT * rows)),
                modifiers: Modifiers::default(),
                touch_phase: TouchPhase::Moved,
                source: ScrollSource::Pointer,
            });
            settle(visual, &workspace, |_| true);
        };
        for (view, mode) in [
            (WorkspaceView::Editor, ThemeMode::Light),
            (WorkspaceView::Docs, ThemeMode::Dark),
        ] {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| this.switch_view(view, window, cx));
                cx.set_reduce_motion(true);
            });
            settle(visual, &workspace, |this| {
                (view != WorkspaceView::Docs || this.docs_tree_complete)
                    && this.rows.iter().any(|row| row.entry.path == Path::new("a"))
            });
            for (folder, child) in [("a", "a/b"), ("a/b", "a/b/00.md"), ("a/c", "a/c/00.md")] {
                visual.update(|window, cx| {
                    workspace.update(cx, |this, cx| {
                        this.toggle_tree_directory(folder.into(), window, cx);
                    });
                });
                settle(visual, &workspace, |this| {
                    this.rows
                        .iter()
                        .any(|row| row.entry.path == Path::new(child))
                });
            }
            let root_id = "workspace-tree-sticky-row:a";
            let b_id = "workspace-tree-sticky-row:a/b";
            let c_id = "workspace-tree-sticky-row:a/c";
            let b_icon_id = "workspace-tree-sticky-collapse:a/b";
            visual.update(|window, cx| {
                apply_theme(mode, cx);
                window.draw(cx).clear(cx);
            });
            assert!(visual.debug_bounds(root_id).is_none());
            let initial = visual.debug_bounds("workspace-tree-row:a").unwrap();
            scroll(visual, initial.center(), 6.);
            let pinned_root = visual.debug_bounds(root_id).unwrap();
            let pinned_b = visual.debug_bounds(b_id).unwrap();
            assert_eq!(pinned_root.top(), initial.top());
            assert_eq!(pinned_b.top(), initial.bottom());
            let folder = visual.debug_bounds(b_icon_id).unwrap();
            assert_eq!(folder.left(), initial.left() + tree_indent(1));
            assert_eq!(pinned_b.left(), folder.right() + px(5.));
            assert!(visual.debug_bounds(c_id).is_none());

            visual.update(|window, _| {
                let scale = window.scale_factor();
                let quads = window.painted_quads();
                let guide_x = initial.left() + px(TREE_GUTTER + 7.);
                let seam = pinned_b.bottom();
                let guide = Palette::for_mode(mode).muted.opacity(0.35);
                for y in [seam - px(4.), seam] {
                    let dash = Bounds::new(point(guide_x, y), size(px(1.), px(2.))).scale(scale);
                    assert!(
                        quads.iter().any(|quad| {
                            quad.bounds == dash && quad.background.as_solid() == Some(guide)
                        }),
                        "missing {mode:?} guide dash at {y:?}"
                    );
                }
                let shadow_bounds = Bounds::new(
                    point(initial.left(), seam),
                    size(initial.size.width, px(5.)),
                )
                .scale(scale);
                let black = Hsla::black();
                let opacity = if mode.is_dark() { 0.24 } else { 0.07 };
                let shadow = linear_gradient(
                    180.,
                    linear_color_stop(black.opacity(opacity), 0.),
                    linear_color_stop(black.opacity(0.), 1.),
                );
                assert!(
                    quads
                        .iter()
                        .any(|quad| { quad.bounds == shadow_bounds && quad.background == shadow }),
                    "missing {mode:?} shadow"
                );
                assert!(
                    !quads.iter().any(|quad| {
                        quad.bounds.size.width == shadow_bounds.size.width
                            && quad.bounds.size.height.0 == scale
                            && (quad.bounds.bottom() == shadow_bounds.top()
                                || quad.bounds.top() == shadow_bounds.top())
                    }),
                    "unexpected divider at the {mode:?} seam"
                );
            });

            let seam = pinned_b.center().y;
            for position in [
                folder.center(),
                pinned_b.center(),
                point(initial.left() + px(4.), seam),
                point(folder.right() + px(2.), seam),
            ] {
                visual.simulate_mouse_move(position, None, Modifiers::default());
                visual.update(|window, cx| {
                    window.draw(cx).clear(cx);
                    let row = Bounds::new(point(initial.left(), pinned_b.top()), initial.size)
                        .scale(window.scale_factor());
                    let hover = Palette::for_mode(mode).hover;
                    assert!(
                        window.painted_quads().iter().any(|quad| {
                            quad.bounds == row && quad.background.as_solid() == Some(hover)
                        }),
                        "missing full-row hover at {position:?} in {mode:?}"
                    );
                });
            }

            visual.simulate_resize(size(px(900.), px(238.)));
            settle(visual, &workspace, |_| true);
            assert!(visual.debug_bounds(root_id).is_none());
            assert_eq!(visual.debug_bounds(b_id).unwrap().top(), initial.top());
            assert_eq!(
                visual.debug_bounds(b_icon_id).unwrap().left(),
                folder.left()
            );
            visual.simulate_resize(size(px(900.), px(560.)));
            settle(visual, &workspace, |_| true);

            scroll(visual, pinned_root.center(), 24.5);
            assert!(visual.debug_bounds(b_id).is_some());
            assert!(visual.debug_bounds(c_id).is_none());
            scroll(visual, pinned_root.center(), 4.5);
            assert_eq!(visual.debug_bounds(root_id), Some(pinned_root));
            assert!(visual.debug_bounds(b_id).is_none());
            let sticky_c = visual.debug_bounds(c_id).unwrap();
            visual.simulate_click(sticky_c.center(), Modifiers::default());
            settle(visual, &workspace, |this| {
                this.tree_keyboard_path == Path::new("a/c")
            });
            let c = visual.debug_bounds("workspace-tree-row:a/c").unwrap();
            assert!(c.top() >= pinned_root.bottom());
            assert!(visual.debug_bounds(c_id).is_none());
            visual.update(|window, cx| {
                let this = workspace.read(cx);
                assert!(this.tree_focus.is_focused(window));
                assert!(
                    this.rows
                        .iter()
                        .any(|row| { row.entry.path == Path::new("a/c") && row.expanded })
                );
            });

            scroll(visual, c.center(), 18.);
            visual.update(|_, cx| cx.set_reduce_motion(false));
            let collapse = visual
                .debug_bounds("workspace-tree-sticky-collapse:a/c")
                .unwrap();
            visual.simulate_click(collapse.center(), Modifiers::default());
            settle(visual, &workspace, |this| {
                this.rows
                    .iter()
                    .any(|row| row.entry.path == Path::new("a/c") && !row.expanded)
            });
            assert!(visual.debug_bounds(c_id).is_none());
            assert!(visual.debug_bounds("workspace-tree-row:a/c").is_some());
            visual.executor().advance_clock(Duration::from_millis(200));
            settle(visual, &workspace, |_| true);
            assert!(visual.debug_bounds(c_id).is_none());
            assert!(visual.debug_bounds("workspace-tree-row:a/c").is_some());
        }

        let root_id = "workspace-tree-sticky-row:a";
        let b_id = "workspace-tree-sticky-row:a/b";
        let top = visual.debug_bounds(root_id).unwrap();
        scroll(visual, top.center(), -14.);
        assert!(visual.debug_bounds(b_id).is_some());
        visual.update(|_, cx| {
            workspace.update(cx, |this, cx| {
                let mut config = this.config.current().clone();
                config.appearance.file_tree_sticky_scroll = false;
                this.config.save(config).unwrap();
                cx.notify();
            });
        });
        settle(visual, &workspace, |_| true);
        assert!(visual.debug_bounds(root_id).is_none());
        assert!(visual.debug_bounds(b_id).is_none());
        assert!(
            !ConfigFile::load(Some(config_path))
                .current()
                .appearance
                .file_tree_sticky_scroll
        );
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn file_tree_context_menus_open_docs_and_source_and_copy_paths() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-qb", "main"])
                .arg(&root)
                .status()?
                .success()
        );
        let path = Path::new("guide.md");
        fs::write(root.join(path), "# Project guide\n")?;
        let source = Path::new("other.md");
        fs::write(root.join(source), "# Other document\n")?;
        let absolute_source = paths::display(&root.canonicalize()?.join(source));

        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(900.), px(620.)));
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                this.open_repository(Location { root, file: None }, window, cx);
            });
        });
        settle(visual, &workspace, |this| {
            this.rows.iter().any(|row| row.entry.path == path)
        });

        let position = visual
            .debug_bounds("workspace-tree-row:guide.md")
            .unwrap()
            .center();
        visual.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(position, MouseButton::Right, Modifiers::default());
        settle(visual, &workspace, |_| true);
        // The first two items open the file as a preview or a regular tab.
        visual.simulate_keystrokes("down down down enter");
        settle(visual, &workspace, |this| {
            this.view == WorkspaceView::Docs && this.doc.is_some()
        });
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.doc.as_ref().unwrap().read(cx).path, path);
            assert_eq!(this.session.document.as_deref(), Some(path));
            assert!(!this.tabs.paths().iter().any(|tab| tab == path));
        });
        settle(visual, &workspace, |this| {
            this.rows.iter().any(|row| row.entry.path == source)
        });
        let open_docs_menu = |visual: &mut VisualTestContext| {
            let position = visual
                .debug_bounds("workspace-tree-row:other.md")
                .unwrap()
                .center();
            visual.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
            visual.simulate_mouse_up(position, MouseButton::Right, Modifiers::default());
            settle(visual, &workspace, |_| true);
        };
        for (keys, expected) in [
            ("up enter", absolute_source.as_str()),
            ("up up enter", "other.md"),
        ] {
            open_docs_menu(visual);
            visual.simulate_keystrokes(keys);
            settle(visual, &workspace, |_| true);
            assert_eq!(
                visual
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some(expected),
            );
        }
        open_docs_menu(visual);
        visual.simulate_keystrokes("down enter");
        settle(visual, &workspace, |this| {
            this.view == WorkspaceView::Editor && this.tabs.active() == Some(source)
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn workspace_tree_opens_image_tabs_and_previews_audio_in_place() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-qb", "main"])
                .arg(&root)
                .status()?
                .success()
        );
        fs::write(root.join("notes.txt"), "Keep this buffer focused.\n")?;
        fs::write(root.join("clip.wav"), b"Invalid audio fixture")?;
        fs::write(root.join("broken.webp"), b"Invalid image fixture")?;
        let image = image::RgbaImage::from_pixel(1600, 960, image::Rgba([93, 146, 223, 160]));
        image.save(root.join("picture.png"))?;
        fs::copy(root.join("picture.png"), root.join("second.png"))?;
        fs::create_dir(root.join("nested"))?;
        fs::copy(root.join("picture.png"), root.join("nested/third.png"))?;

        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(900.), px(620.)));
        let root = test_repository_location(root);
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root,
                        file: Some("notes.txt".into()),
                    },
                    window,
                    cx,
                );
            });
        });
        let notes = Path::new("notes.txt");
        let picture = Path::new("picture.png");
        let broken = Path::new("broken.webp");
        settle(visual, &workspace, |this| this.active_file().is_some());
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.keep_tab(notes, cx);
                this.finish_tab_selection(Some(notes), window, cx);
            })
        });

        let audio_row = visual
            .debug_bounds("workspace-tree-row:clip.wav")
            .unwrap()
            .center();
        let focused = visual.update(|window, cx| window.focused(cx));
        assert!(focused.is_some());
        visual.simulate_click(audio_row, Modifiers::default());
        visual.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: audio_row,
            click_count: 2,
            ..Default::default()
        });
        visual.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: audio_row,
            click_count: 2,
            ..Default::default()
        });
        visual.update(|window, cx| {
            assert_eq!(workspace.read(cx).tabs.paths(), [notes.to_owned()]);
            assert_eq!(window.focused(cx), focused);
        });

        let image_row = visual
            .debug_bounds("workspace-tree-row:picture.png")
            .unwrap()
            .center();
        let hover_delay = visual
            .update(|_, cx| gpui_component::tooltip::TooltipDefaults::global(cx).show_delay());
        visual.simulate_mouse_move(image_row, None, Modifiers::default());
        visual
            .executor()
            .advance_clock(hover_delay + Duration::from_millis(32));
        settle(visual, &workspace, |this| this.active_file().is_some());
        let hover = visual
            .debug_bounds("image-hover-preview")
            .expect("tree image hover");
        assert!(hover.left() > image_row.x);
        assert_eq!(hover.size, size(px(320.), px(240.)));
        assert!(visual.debug_bounds("image-hover-loaded").is_some());

        // A redraw while the pointer stays put must keep the decoded image visible.
        settle(visual, &workspace, |this| this.active_file().is_some());
        assert!(visual.debug_bounds("image-hover-loaded").is_some());

        visual.simulate_click(image_row, Modifiers::default());
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(picture)
        });
        let viewport = visual.debug_bounds("workspace-image-viewport").unwrap();
        let controls = visual.debug_bounds("workspace-image-controls").unwrap();
        assert_eq!(viewport.top(), controls.top());
        assert!(viewport.contains(&controls.center()));
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert!(this.tabs.is_preview(picture));
            assert!(this.active_file().is_none());
            assert!(!this.tab_is_dirty(picture));
            assert!(this.image_preview.is_none());
        });

        let mut scroll = ScrollEvent {
            position: viewport.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(13.))),
            modifiers: Modifiers {
                control: true,
                ..Default::default()
            },
            source: ScrollSource::Touchpad,
            touch_phase: TouchPhase::Started,
        };
        let zoom = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| workspace.read(cx).active_image_tab().unwrap().1.zoom())
        };
        let tick = |visual: &mut VisualTestContext| {
            visual.executor().advance_clock(Duration::from_millis(16));
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
        };
        for _ in 0..3 {
            visual.simulate_event(scroll.clone());
            scroll.touch_phase = TouchPhase::Moved;
            tick(visual);
        }
        assert_eq!(zoom(visual), None);
        visual.simulate_event(scroll.clone());
        for _ in 0..5 {
            tick(visual);
        }
        assert_eq!(zoom(visual), Some(1.));
        scroll.delta = ScrollDelta::Lines(point(0., 1.));
        scroll.source = ScrollSource::Pointer;
        visual.simulate_event(scroll);
        assert_eq!(zoom(visual), Some(1.25));

        let offset = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| workspace.read(cx).active_image_tab().unwrap().1.offset())
        };
        let before = offset(visual);
        visual.simulate_event(ScrollEvent {
            position: viewport.center(),
            delta: ScrollDelta::Pixels(point(px(21.), px(-19.))),
            modifiers: Modifiers::default(),
            source: ScrollSource::Touchpad,
            touch_phase: TouchPhase::Started,
        });
        for _ in 0..5 {
            tick(visual);
        }
        assert_eq!(offset(visual) - before, point(px(21.), px(-19.)));
        let before = offset(visual);
        visual.simulate_mouse_down(viewport.center(), MouseButton::Left, Modifiers::default());
        visual.simulate_mouse_move(
            viewport.center() + point(px(-7.), px(12.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        visual.simulate_mouse_up(viewport.center(), MouseButton::Left, Modifiers::default());
        assert_eq!(offset(visual) - before, point(px(-7.), px(12.)));
        visual.simulate_keystrokes("right");
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(Path::new("second.png"))
        });
        visual.simulate_keystrokes("down");
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(broken)
        });
        visual.simulate_keystrokes("up left");
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(picture)
        });

        let broken_row = visual
            .debug_bounds("workspace-tree-row:broken.webp")
            .unwrap()
            .center();
        visual.simulate_click(broken_row, Modifiers::default());
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(broken)
        });
        assert!(visual.debug_bounds("workspace-image-status").is_some());
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert!(!this.tabs.paths().iter().any(|path| path == picture));
            assert!(!this.images.contains_key(picture));
        });

        visual.simulate_click(image_row, Modifiers::default());
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(picture)
        });
        visual.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: image_row,
            click_count: 2,
            ..Default::default()
        });
        visual.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: image_row,
            click_count: 2,
            ..Default::default()
        });
        settle(visual, &workspace, |this| !this.tabs.is_preview(picture));
        visual.update(|_, cx| {
            let (_, tab) = workspace.read(cx).active_image_tab().unwrap();
            assert_eq!(tab.sampling(), ImageSampling::Linear);
        });
        let control = visual.debug_bounds("workspace-image-sampling").unwrap();
        assert_eq!(control.size.height, px(26.));
        visual.simulate_click(control.center(), Modifiers::default());
        settle(visual, &workspace, |_| true);
        visual.simulate_keystrokes("down down enter");
        settle(visual, &workspace, |this| {
            this.active_image_tab()
                .is_some_and(|(_, tab)| tab.sampling() == ImageSampling::Nearest)
        });

        let second = Path::new("second.png");
        let second_row = visual
            .debug_bounds("workspace-tree-row:second.png")
            .unwrap();
        visual.simulate_click(second_row.center(), Modifiers::default());
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(second)
        });
        visual.update(|_, cx| {
            let (_, tab) = workspace.read(cx).active_image_tab().unwrap();
            assert_eq!(tab.sampling(), ImageSampling::Linear);
        });
        visual.simulate_click(image_row, Modifiers::default());
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(picture)
        });
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert_eq!(
                    this.active_image_tab().unwrap().1.sampling(),
                    ImageSampling::Nearest
                );
                assert!(!this.tabs.is_preview(picture));
                this.close_tabs(vec![second.to_owned(), picture.to_owned()], window, cx);
                assert_eq!(this.tabs.active(), Some(notes));
                assert!(!this.images.contains_key(picture));
                assert!(this.request_window_close(window, cx));
            });
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn tree_expansion_survives_switching_worktrees_and_reopening() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let directory = fixture.path().canonicalize()?;
        let root = directory.join("repo");
        let other = directory.join("other");
        for directory in ["shared/deep", "editor/nested", "docs"] {
            fs::create_dir_all(root.join(directory))?;
            fs::write(root.join(directory).join("page.md"), "# Page")?;
        }
        fs::write(root.join("README.md"), "# Project")?;
        let git = |args: &[&str]| -> Result<()> {
            assert!(
                std::process::Command::new("git")
                    .current_dir(&root)
                    .args(args)
                    .status()?
                    .success()
            );
            Ok(())
        };
        git(&["init", "-qb", "main"])?;
        git(&["add", "."])?;
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
        git(&["worktree", "add", "-qb", "other", other.to_str().unwrap()])?;

        let mut cx = TestAppContext::single();
        cx.update(gpui_component::init);
        fn open_window<'a>(
            cx: &'a mut TestAppContext,
            directory: &Path,
        ) -> Result<(Entity<Workspace>, &'a mut VisualTestContext)> {
            let store = Arc::new(Store::new(directory.join("state"))?);
            let config = ConfigFile::load(Some(directory.join("config.toml")));
            let mut workspace = None;
            let (_, visual) = cx.add_window_view(|window, cx| {
                cx.set_reduce_motion(true);
                let view = cx.new(|cx| {
                    Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
                });
                workspace = Some(view.clone());
                gpui_component::Root::new(view, window, cx)
            });
            visual.simulate_resize(size(px(1024.), px(650.)));
            Ok((workspace.unwrap(), visual))
        }
        let open = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>, root: &Path| {
            visual.update(|window, cx| {
                window.activate_window();
                workspace.update(cx, |this, cx| {
                    this.open_repository(
                        Location {
                            root: root.to_owned(),
                            file: None,
                        },
                        window,
                        cx,
                    );
                });
            });
            settle(visual, workspace, |this| {
                this.repository
                    .as_ref()
                    .is_some_and(|repo| repo.root == root)
                    && this.repository_ui.opening.is_none()
            });
        };
        let switch_docs = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>| {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.switch_view(WorkspaceView::Docs, window, cx)
                });
            });
            settle(visual, workspace, |this| this.docs_tree_complete);
        };
        let has_row = |workspace: &Workspace, path: &str| {
            workspace
                .rows
                .iter()
                .any(|row| row.entry.path == Path::new(path))
        };
        let click = |visual: &mut VisualTestContext, selector| {
            let bounds = visual.debug_bounds(selector).unwrap();
            visual.simulate_click(bounds.center(), Modifiers::default());
        };

        let (workspace, visual) = open_window(&mut cx, &directory)?;
        open(visual, &workspace, &root);
        for (directory, child) in [
            ("workspace-tree-row:shared", "shared/deep"),
            ("workspace-tree-row:shared/deep", "shared/deep/page.md"),
            ("workspace-tree-row:editor", "editor/nested"),
            ("workspace-tree-row:editor/nested", "editor/nested/page.md"),
        ] {
            click(visual, directory);
            settle(visual, &workspace, |this| has_row(this, child));
        }
        click(visual, "workspace-tree-row:shared");
        settle(visual, &workspace, |this| !has_row(this, "shared/deep"));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| this.rescan_workspace(window, cx));
        });
        settle(visual, &workspace, |this| !this.workspace_refreshing);
        switch_docs(visual, &workspace);
        click(visual, "workspace-tree-row:docs");
        settle(visual, &workspace, |this| has_row(this, "docs/page.md"));

        open(visual, &workspace, &other);
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert!(this.expanded.is_empty() && this.docs_expanded.is_empty());
        });
        open(visual, &workspace, &root);
        settle(visual, &workspace, |this| has_row(this, "docs/page.md"));

        let (reopened, visual) = open_window(&mut cx, &directory)?;
        open(visual, &reopened, &root);
        settle(visual, &reopened, |this| {
            has_row(this, "editor/nested/page.md")
        });
        visual.update(|_, cx| {
            let this = reopened.read(cx);
            assert!(!has_row(this, "shared/deep"));
            assert!(!has_row(this, "docs/page.md"));
        });
        click(visual, "workspace-tree-row:shared");
        settle(visual, &reopened, |this| {
            has_row(this, "shared/deep/page.md")
        });

        switch_docs(visual, &reopened);
        visual.update(|_, cx| {
            let this = reopened.read(cx);
            assert!(has_row(this, "docs/page.md"));
            assert!(!has_row(this, "editor/nested"));
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn workspace_lazy_tree_loads_one_layer_and_reveals_active_tab() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q", "--"])
                .arg(&root)
                .status()?
                .success()
        );
        fs::write(root.join(".gitignore"), "ignored/\n")?;
        fs::write(root.join(".visible"), "")?;
        fs::create_dir_all(root.join("ignored/deep"))?;
        fs::create_dir_all(root.join("ignored/deep/unopened"))?;
        fs::write(root.join("ignored/deep/unopened/untouched.txt"), "")?;
        fs::write(root.join("ignored/deep/z-target.txt"), "target")?;
        for index in 0..60 {
            fs::write(root.join(format!("ignored/deep/file-{index:02}.txt")), "")?;
        }
        let large_fixture = env::var_os("STROLL_TEST_LARGE_IGNORED_TREE").is_some();
        if large_fixture {
            fs::create_dir(root.join("ignored/bulk"))?;
            for dir in 0..2_000 {
                let parent = root.join(format!("ignored/bulk/dir-{dir:04}"));
                fs::create_dir(&parent)?;
                for file in 0..50 {
                    fs::write(parent.join(format!("file-{file:02}.txt")), "")?;
                }
            }
        }

        let mut cx = TestAppContext::single();
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut workspace = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1024.), px(650.)));
        let visual = &mut visual;
        let location = test_repository_location(root);
        let remote = location.to_string_lossy().starts_with("ssh://");
        let start = std::time::Instant::now();
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root: location,
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        settle(visual, &workspace, |this| {
            this.rows
                .iter()
                .any(|row| row.entry.path == Path::new(".visible"))
        });
        assert!(visual.debug_bounds("workspace-tree-row:.visible").is_some());
        let first_frame = start.elapsed();
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            let repo = this
                .repository_snapshot
                .as_ref()
                .or(this.repository.as_ref())
                .unwrap();
            assert_eq!(
                repo.children.keys().cloned().collect::<Vec<_>>(),
                [PathBuf::new()]
            );
            assert!(
                this.rows
                    .iter()
                    .any(|row| row.entry.path == Path::new("ignored") && row.dimmed)
            );
            assert!(
                this.rows
                    .iter()
                    .any(|row| row.entry.path == Path::new(".visible") && !row.dimmed)
            );
        });
        if large_fixture {
            settle(visual, &workspace, |this| this.repository_watch_active);
            let watch_ready = start.elapsed();
            settle(visual, &workspace, |this| {
                !this.workspace_refreshing && this.tree_status.files.contains_key(".visible")
            });
            eprintln!(
                "workspace SSH={remote}, 100000 bulk files, first frame {:.1} ms, watch ready {:.1} ms, first refresh/status {:.1} ms",
                first_frame.as_secs_f64() * 1_000.,
                watch_ready.as_secs_f64() * 1_000.,
                start.elapsed().as_secs_f64() * 1_000.
            );
        }

        let ignored = visual.debug_bounds("workspace-tree-row:ignored").unwrap();
        visual.simulate_click(ignored.center(), Modifiers::default());
        settle(visual, &workspace, |this| {
            this.rows
                .iter()
                .any(|row| row.entry.path == Path::new("ignored/deep"))
        });
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.repository_snapshot.as_ref().unwrap().children.len(), 2);
            assert!(
                this.rows
                    .iter()
                    .any(|row| row.entry.path == Path::new("ignored/deep") && !row.dimmed)
            );
        });
        visual.simulate_click(ignored.center(), Modifiers::default());

        let target = Path::new("ignored/deep/z-target.txt");
        let dotfile = Path::new(".visible");
        // Restoring a tab must discover its ancestor directories even when they
        // were collapsed and the target was never reached by expanding the tree.
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_file(target.to_owned(), false, window, cx)
            })
        });
        settle(visual, &workspace, |this| {
            this.rows.iter().any(|row| row.entry.path == target)
        });
        settle(visual, &workspace, |_| true);
        let target_bounds = visual
            .debug_bounds("workspace-tree-row:ignored/deep/z-target.txt")
            .unwrap();
        assert!(target_bounds.bottom() <= px(650.));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let repo = this.repository_snapshot.as_ref().unwrap();
                assert!(repo.children.contains_key(Path::new("ignored/deep")));
                assert!(
                    !repo
                        .children
                        .contains_key(Path::new("ignored/deep/unopened"))
                );
                assert!(!repo.children.contains_key(Path::new("ignored/bulk")));
                this.open_file(dotfile.to_owned(), false, window, cx);
            });
        });
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(dotfile)
        });
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.expanded.clear();
                this.rebuild_tree();
                this.activate_tab(target, window, cx);
            });
        });
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(target)
        });
        settle(visual, &workspace, |_| true);
        assert!(
            visual
                .debug_bounds("workspace-tree-row:ignored/deep/z-target.txt")
                .is_some()
        );
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert!(
                this.active_file()
                    .unwrap()
                    .editor
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn workspace_tree_drag_moves_drafts_and_imports_external_entries() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        fs::create_dir(root.join("target"))?;
        fs::write(root.join("source.txt"), "on disk")?;
        let folder = fixture.path().join("folder from desktop");
        fs::create_dir(&folder)?;
        fs::write(folder.join("data.bin"), [0, 255])?;
        let local_file = fixture.path().join("clipboard source.txt");
        fs::write(&local_file, "from desktop")?;

        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut workspace = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1024.), px(700.)));
        let open_root = test_repository_location(root.clone());
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root: open_root,
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        settle(&mut visual, &workspace, |this| this.repository_watch_active);

        let source = visual
            .debug_bounds("workspace-tree-row:source.txt")
            .unwrap()
            .center();
        let target = visual
            .debug_bounds("workspace-tree-row:target")
            .unwrap()
            .center();
        visual.simulate_click(source, Modifiers::default());
        settle(&mut visual, &workspace, |this| {
            this.tabs.active() == Some(Path::new("source.txt"))
        });
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert!(this.tree_focus.is_focused(window));
                let editor = this.active_file().unwrap().editor.clone();
                editor.update(cx, |editor, cx| editor.set_value("unsaved", window, cx));
                this.editor_changed(editor, window, cx);
            })
        });

        let modifiers = Modifiers::default();
        visual.simulate_mouse_move(source, None, modifiers);
        visual.simulate_mouse_down(source, MouseButton::Left, modifiers);
        visual.simulate_mouse_move(
            source + point(px(30.), px(0.)),
            MouseButton::Left,
            modifiers,
        );
        settle(&mut visual, &workspace, |_| true);
        visual.simulate_mouse_move(target, MouseButton::Left, modifiers);
        visual.simulate_mouse_up(target, MouseButton::Left, modifiers);
        settle(&mut visual, &workspace, |this| {
            !this.tree_busy && this.tabs.active() == Some(Path::new("target/source.txt"))
        });
        assert!(!root.join("source.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("target/source.txt"))?,
            "on disk"
        );
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.active_file().unwrap().committed.to_string(), "unsaved");
        });

        let remote = visual.update(|_, cx| {
            workspace
                .read(cx)
                .repository
                .as_ref()
                .unwrap()
                .ssh
                .is_some()
        });
        if remote {
            if !visual.update(|_, cx| workspace.read(cx).expanded.contains(Path::new("target"))) {
                visual.simulate_click(target, modifiers);
            }
            settle(&mut visual, &workspace, |this| {
                this.rows
                    .iter()
                    .any(|row| row.entry.path == Path::new("target/source.txt"))
            });
            let moved = visual
                .debug_bounds("workspace-tree-row:target/source.txt")
                .unwrap()
                .center();
            visual.simulate_click(moved, modifiers);
            settle(&mut visual, &workspace, |_| true);
            visual.simulate_keystrokes("ctrl-c");
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.focus_tree(PathBuf::new(), window, cx);
                })
            });
            settle(&mut visual, &workspace, |_| true);
            visual.simulate_keystrokes("ctrl-v");
            settle(&mut visual, &workspace, |_| {
                root.join("source.txt").is_file()
            });
            assert_eq!(fs::read_to_string(root.join("source.txt"))?, "on disk");
            return Ok(());
        }

        let target = visual
            .debug_bounds("workspace-tree-row:target")
            .unwrap()
            .center();
        visual.simulate_event(FileDropEvent::Entered {
            position: target,
            paths: ExternalPaths([folder.clone(), local_file.clone()].into_iter().collect()),
        });
        visual.simulate_event(FileDropEvent::Submit { position: target });
        settle(&mut visual, &workspace, |_| {
            root.join("target/clipboard source.txt").is_file()
        });
        assert_eq!(
            fs::read(root.join("target/folder from desktop/data.bin"))?,
            [0, 255]
        );
        assert_eq!(
            fs::read_to_string(root.join("target/clipboard source.txt"))?,
            "from desktop"
        );
        assert!(folder.is_dir());
        assert!(local_file.is_file());
        visual.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).tabs.paths(),
                [PathBuf::from("target/source.txt")]
            );
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn editor_file_tree_navigates_with_arrow_keys() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        let nested = root.join("folder/nested");
        fs::create_dir_all(&nested)?;
        for index in 0..40 {
            fs::write(nested.join(format!("{index:02}.txt")), "child\n")?;
        }
        fs::write(root.join("folder/z.txt"), "sibling\n")?;
        fs::write(root.join("notes.txt"), "open file\n")?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-qb", "main"])
                .arg(&root)
                .status()?
                .success()
        );

        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(900.), px(500.)));
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root,
                        file: Some("notes.txt".into()),
                    },
                    window,
                    cx,
                );
            });
        });
        settle(visual, &workspace, |this| this.active_file().is_some());
        visual.simulate_keystrokes("down");
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert!(!this.tree_focus.is_focused(window));
                assert!(this.tree_keyboard_path.as_os_str().is_empty());
                this.focus_tree(PathBuf::new(), window, cx);
            });
        });
        settle(visual, &workspace, |_| true);

        let key = |visual: &mut VisualTestContext, keys: &str, selected: &str| {
            visual.simulate_keystrokes(keys);
            settle(visual, &workspace, |_| true);
            visual.update(|window, cx| {
                let this = workspace.read(cx);
                assert!(this.tree_focus.is_focused(window));
                assert_eq!(this.tree_keyboard_path, Path::new(selected), "{keys}");
                assert_eq!(this.tabs.active(), Some(Path::new("notes.txt")));
            });
        };
        let expanded = |visual: &mut VisualTestContext, path: &str| {
            visual.update(|_, cx| workspace.read(cx).expanded.contains(Path::new(path)))
        };

        key(visual, "up up left", "folder");
        key(visual, "right", "folder");
        settle(visual, &workspace, |this| {
            this.rows
                .iter()
                .any(|row| row.entry.path == Path::new("folder/nested"))
        });
        key(visual, "right", "folder/nested");
        key(visual, "left", "folder");
        assert!(expanded(visual, "folder"));
        key(visual, "right right", "folder/nested");
        settle(visual, &workspace, |this| {
            this.rows
                .iter()
                .any(|row| row.entry.path == Path::new("folder/nested/00.txt"))
        });
        key(visual, "right right", "folder/nested/00.txt");
        key(visual, "left", "folder/nested");
        assert!(expanded(visual, "folder/nested"));
        key(visual, "right", "folder/nested/00.txt");

        let down = vec!["down"; 39].join(" ");
        key(visual, &down, "folder/nested/39.txt");
        let row = visual
            .debug_bounds("workspace-tree-row:folder/nested/39.txt")
            .unwrap();
        let viewport = visual.update(|_, cx| {
            workspace
                .read(cx)
                .tree_scroll
                .0
                .borrow()
                .base_handle
                .bounds()
        });
        assert!(row.top() >= viewport.top() && row.bottom() <= viewport.bottom());

        key(visual, "down", "folder/z.txt");
        key(visual, "up left", "folder/nested");
        assert!(expanded(visual, "folder/nested"));
        key(visual, "left", "folder/nested");
        assert!(!expanded(visual, "folder/nested"));
        key(visual, "down down down", "notes.txt");

        let sibling = Path::new("folder/z.txt");
        visual.simulate_keystrokes("up space down");
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(sibling)
        });
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert!(this.tabs.is_preview(sibling));
            assert_eq!(this.tree_keyboard_path, Path::new("notes.txt"));
            assert!(this.tree_focus.is_focused(window));
        });

        let open = if cfg!(target_os = "macos") {
            "cmd-down"
        } else {
            "enter"
        };
        visual.simulate_keystrokes("up");
        visual.simulate_keystrokes(open);
        settle(visual, &workspace, |this| !this.tabs.is_preview(sibling));
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.tabs.active(), Some(sibling));
            assert!(
                this.active_file()
                    .unwrap()
                    .editor
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn file_tree_shortcuts_and_context_menu_hints() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-qb", "main"])
                .arg(&root)
                .status()?
                .success()
        );
        fs::write(root.join("original.txt"), "original\n")?;
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1024.), px(700.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root: root.clone(),
                        file: Some("original.txt".into()),
                    },
                    window,
                    cx,
                )
            });
        });
        settle(visual, &workspace, |this| {
            this.repository_watch_active && this.active_file().is_some()
        });
        let original = visual
            .debug_bounds("workspace-tree-row:original.txt")
            .unwrap()
            .center();
        visual.simulate_mouse_down(original, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(original, MouseButton::Right, Modifiers::default());
        settle(visual, &workspace, |_| true);
        for shortcut in if cfg!(target_os = "macos") {
            ["kbd:cmd-c", "kbd:cmd-x", "kbd:enter", "kbd:cmd-backspace"]
        } else {
            ["kbd:ctrl-c", "kbd:ctrl-x", "kbd:f2", "kbd:delete"]
        } {
            assert!(
                visual.debug_bounds(shortcut).is_some(),
                "file context menu must paint {shortcut}"
            );
        }
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.focus_tree("original.txt".into(), window, cx);
            });
        });
        settle(visual, &workspace, |_| true);
        visual.simulate_keystrokes(if cfg!(target_os = "macos") {
            "enter"
        } else {
            "f2"
        });
        settle(visual, &workspace, |this| this.tree_edit.is_some());
        visual.simulate_input("keyboard.tt");
        visual.simulate_keystrokes("left space");
        visual.simulate_input("x");
        visual.simulate_keystrokes("left backspace");
        // File tree shortcuts must not intercept editing in the rename input.
        visual.simulate_keystrokes("f2");
        assert!(visual.update(|_, cx| workspace.read(cx).tree_edit.is_some()));
        visual.simulate_keystrokes("enter");
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(Path::new("keyboard.txt"))
        });
        assert!(root.join("keyboard.txt").is_file());
        assert!(!root.join("original.txt").exists());
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.focus_tree("keyboard.txt".into(), window, cx)
            });
        });
        settle(visual, &workspace, |_| true);
        visual.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-backspace"
        } else {
            "delete"
        });
        settle(visual, &workspace, |this| !this.tree_busy);
        assert!(visual.update(|window, cx| window.has_active_dialog(cx)));
        assert!(
            root.join("keyboard.txt").is_file(),
            "delete requires confirmation"
        );
        visual.update(|window, cx| window.close_dialog(cx));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[::core::prelude::v1::test]
    fn workspace_file_tree_watcher_and_inline_edit() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-qb", "main"])
                .arg(&root)
                .status()?
                .success()
        );
        fs::write(root.join(".gitignore"), "ignored/\n*.log\n")?;
        fs::write(root.join(".visible"), "dotfile\n")?;
        fs::write(root.join(".ignored.log"), "ignored dotfile\n")?;
        fs::write(root.join("original.txt"), "initial\n")?;
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut workspace = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        let visual = &mut visual;
        visual.simulate_resize(size(px(1024.), px(700.)));
        let open_root = test_repository_location(root.clone());
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root: open_root,
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        settle(visual, &workspace, |this| this.repository_watch_active);
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_file("original.txt".into(), false, window, cx)
            });
        });
        settle(visual, &workspace, |this| {
            this.files.contains_key(Path::new("original.txt"))
        });

        fs::create_dir(root.join("ignored"))?;
        settle(visual, &workspace, |this| {
            this.rows
                .iter()
                .any(|row| row.entry.path == Path::new("ignored") && row.dimmed)
        });
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            for name in [".gitignore", ".visible"] {
                assert!(
                    this.rows
                        .iter()
                        .any(|row| { row.entry.path == Path::new(name) && !row.dimmed })
                );
            }
            assert!(
                this.rows
                    .iter()
                    .any(|row| { row.entry.path == Path::new(".ignored.log") && row.dimmed })
            );
        });
        let ignored = visual.debug_bounds("workspace-tree-row:ignored").unwrap();
        visual.simulate_click(ignored.center(), Modifiers::default());
        fs::write(root.join("ignored/notes.txt"), "ignored file\n")?;
        settle(visual, &workspace, |this| {
            this.rows.iter().any(|row| {
                row.entry.path == Path::new("ignored/notes.txt") && row.entry.ignored && !row.dimmed
            })
        });
        let notes = visual
            .debug_bounds("workspace-tree-row:ignored/notes.txt")
            .unwrap();
        visual.simulate_click(notes.center(), Modifiers::default());
        settle(visual, &workspace, |this| {
            this.active_file()
                .is_some_and(|file| file.saved == "ignored file\n")
        });
        fs::write(root.join("ignored/notes.txt"), "updated ignored file\n")?;
        settle(visual, &workspace, |this| {
            this.active_file()
                .is_some_and(|file| file.saved == "updated ignored file\n")
        });
        fs::write(root.join(".gitignore"), "ignored/\n")?;
        settle(visual, &workspace, |this| {
            this.rows
                .iter()
                .any(|row| row.entry.path == Path::new(".ignored.log") && !row.dimmed)
        });
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_file("original.txt".into(), false, window, cx)
            });
        });
        settle(visual, &workspace, |this| {
            this.active_file()
                .is_some_and(|file| file.path == Path::new("original.txt"))
        });
        let revision = visual.update(|_, cx| {
            workspace.update(cx, |this, _| {
                this.saving.insert(PathBuf::from("original.txt"));
                this.active_file().unwrap().sync_revision
            })
        });
        fs::create_dir(root.join("empty"))?;
        fs::write(root.join("external.tmp"), "external edit\n")?;
        fs::rename(root.join("external.tmp"), root.join("original.txt"))?;
        settle(visual, &workspace, |this| {
            this.active_file()
                .is_some_and(|file| file.sync_revision >= revision + 2)
        });
        visual.update(|_, cx| {
            workspace.update(cx, |this, _| {
                assert_ne!(this.active_file().unwrap().saved, "external edit\n");
                this.saving.remove(Path::new("original.txt"));
            });
        });
        settle(visual, &workspace, |this| {
            this.rows
                .iter()
                .any(|row| row.entry.path == Path::new("empty"))
                && this
                    .active_file()
                    .is_some_and(|file| file.saved == "external edit\n")
        });
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let editor = this.active_file().unwrap().editor.clone();
                editor.update(cx, |editor, cx| {
                    editor.set_value("my unsaved draft\n", window, cx)
                });
                this.editor_changed(editor, window, cx);
            });
        });

        settle(visual, &workspace, |_| true);
        let original = visual
            .debug_bounds("workspace-tree-row:original.txt")
            .unwrap();
        visual.simulate_mouse_move(original.center(), None, Modifiers::default());
        visual.simulate_mouse_down(original.center(), MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(original.center(), MouseButton::Right, Modifiers::default());
        settle(visual, &workspace, |_| true);
        for shortcut in if cfg!(target_os = "macos") {
            ["kbd:cmd-c", "kbd:cmd-x", "kbd:enter", "kbd:cmd-backspace"]
        } else {
            ["kbd:ctrl-c", "kbd:ctrl-x", "kbd:f2", "kbd:delete"]
        } {
            assert!(
                visual.debug_bounds(shortcut).is_some(),
                "file context menu must paint {shortcut}"
            );
        }
        visual.simulate_keystrokes("down down down enter");
        assert!(
            visual.did_prompt_for_new_path(),
            "Save As must open the platform save dialog"
        );
        let copy = fixture.path().join("copy.txt");
        visual.simulate_new_path_selection(|_| Some(copy.clone()));
        settle(visual, &workspace, |this| {
            assert_eq!(this.tabs.active(), Some(Path::new("original.txt")));
            copy.is_file()
        });
        assert_eq!(fs::read_to_string(copy)?, "my unsaved draft\n");
        assert_eq!(
            fs::read_to_string(root.join("original.txt"))?,
            "external edit\n"
        );

        fs::rename(root.join("original.txt"), root.join("renamed.txt"))?;
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(Path::new("renamed.txt"))
                && this
                    .active_file()
                    .is_some_and(|file| file.committed == "my unsaved draft\n")
                && this
                    .repository
                    .as_ref()
                    .is_some_and(|repo| this.store.has_draft(repo, Path::new("renamed.txt")))
        });
        fs::remove_file(root.join("renamed.txt"))?;
        settle(visual, &workspace, |this| {
            this.active_file().is_some_and(|file| file.missing)
        });
        fs::write(root.join("renamed.txt"), "new external edit\n")?;
        settle(visual, &workspace, |this| {
            this.active_file().is_some_and(|file| {
                !file.missing && file.document.conflict && file.committed == "my unsaved draft\n"
            })
        });

        let folder = visual
            .debug_bounds("workspace-tree-row:empty")
            .expect("empty folder is visible");
        visual.simulate_mouse_down(folder.center(), MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(folder.center(), MouseButton::Right, Modifiers::default());
        settle(visual, &workspace, |_| true);
        visual.simulate_keystrokes("down enter");
        settle(visual, &workspace, |this| this.tree_edit.is_some());
        visual.simulate_input("created.txt");
        visual.simulate_keystrokes("enter");
        settle(visual, &workspace, |this| {
            this.tabs.active() == Some(Path::new("empty/created.txt"))
        });
        assert!(root.join("empty/created.txt").is_file());
        Ok(())
    }
}
