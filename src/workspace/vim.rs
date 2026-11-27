mod go;
pub(super) mod source;

use std::{path::PathBuf, time::Duration};

use gpui::*;
use gpui_component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, ThemeStyled as _, WindowExt as _, h_flex,
    input::{self, EditorMode, InputModeKind as _},
    v_flex,
};
use rust_i18n::t;

use super::{
    OpenFile, Palette, Workspace, WorkspaceView, baseline_aligned, file_tree::NavigateTree,
};

pub(super) use source::Source;

const HINT_DELAY: Duration = Duration::from_millis(300);
pub(super) const STATUS_HEIGHT: f32 = 26.;
const SPACE: &str = "␣";
const STATUS_BUTTON_SIZE: Pixels = px(22.);
const STATUS_ICON_SIZE: Pixels = px(17.5);
const STATUS_ITEM_GAP: Pixels = px(4.);
const MODE_HEIGHT: Pixels = px(18.);
const PENDING_SAMPLE: &str = "…0000000␣";

#[derive(Default)]
pub(super) struct WorkspaceState {
    hint: Option<KeyHint>,
    return_from_tree: Option<FocusHandle>,
    reader_prefix: bool,
    preview_command: Option<PreviewCommand>,
    wide_status: bool,
    search: Option<source::Find>,
    pub(super) close_preference: Option<(PathBuf, PathBuf)>,
}

impl WorkspaceState {
    pub(super) fn forget_tree_origin(&mut self) {
        self.return_from_tree = None;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HintKind {
    Leader,
    Go(GoScope),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GoScope {
    Source,
    Reader,
    Image,
    Diff,
    ChangedFiles,
}

struct PreviewCommand {
    pending: String,
    _focus_out: Subscription,
}

struct KeyHint {
    kind: HintKind,
    focus: FocusHandle,
    prefix: String,
    visible: bool,
    _focus_out: Subscription,
    timer: Task<()>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Command {
    Files,
    Text,
    DocumentSymbols,
    WorkspaceSymbols,
    View(WorkspaceView),
    Tree,
    Git,
    Status,
    Diff,
    Log,
    Terminal,
    Close,
    Comment,
    CodeActions,
    Rename,
    Format,
    Go(GoCommand),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GoCommand {
    Top,
    Definition,
    References,
    Comment,
    Join,
    Tab(bool),
    File,
    Docs,
}

struct Binding {
    keys: &'static str,
    command: Command,
    label: &'static str,
}

const LEADER_BINDINGS: &[Binding] = &[
    Binding {
        keys: " ",
        command: Command::Files,
        label: "vim.files",
    },
    Binding {
        keys: "ff",
        command: Command::Files,
        label: "vim.files",
    },
    Binding {
        keys: "ft",
        command: Command::Terminal,
        label: "vim.terminal",
    },
    Binding {
        keys: "sg",
        command: Command::Text,
        label: "vim.text",
    },
    Binding {
        keys: "ss",
        command: Command::DocumentSymbols,
        label: "locator.document_symbols",
    },
    Binding {
        keys: "sS",
        command: Command::WorkspaceSymbols,
        label: "locator.workspace_symbols",
    },
    Binding {
        keys: "1",
        command: Command::View(WorkspaceView::Docs),
        label: "common.docs",
    },
    Binding {
        keys: "2",
        command: Command::View(WorkspaceView::Editor),
        label: "common.workspace",
    },
    Binding {
        keys: "3",
        command: Command::View(WorkspaceView::Git),
        label: "vim.git",
    },
    Binding {
        keys: "e",
        command: Command::Tree,
        label: "vim.tree",
    },
    Binding {
        keys: "gg",
        command: Command::Git,
        label: "vim.git",
    },
    Binding {
        keys: "gs",
        command: Command::Status,
        label: "vim.status",
    },
    Binding {
        keys: "gd",
        command: Command::Diff,
        label: "vim.diff",
    },
    Binding {
        keys: "gl",
        command: Command::Log,
        label: "git.history",
    },
    Binding {
        keys: "bd",
        command: Command::Close,
        label: "vim.close_tab",
    },
    Binding {
        keys: "/",
        command: Command::Comment,
        label: "vim.comment",
    },
    Binding {
        keys: "ca",
        command: Command::CodeActions,
        label: "vim.code_actions",
    },
    Binding {
        keys: "cr",
        command: Command::Rename,
        label: "vim.rename",
    },
    Binding {
        keys: "cf",
        command: Command::Format,
        label: "vim.format",
    },
];

const GO_BINDINGS: &[Binding] = &[
    Binding {
        keys: "g",
        command: Command::Go(GoCommand::Top),
        label: "vim.go_top",
    },
    Binding {
        keys: "d",
        command: Command::Go(GoCommand::Definition),
        label: "vim.go_to_definition",
    },
    Binding {
        keys: "r",
        command: Command::Go(GoCommand::References),
        label: "lsp.find_references",
    },
    Binding {
        keys: "cc",
        command: Command::Go(GoCommand::Comment),
        label: "vim.comment",
    },
    Binding {
        keys: "J",
        command: Command::Go(GoCommand::Join),
        label: "vim.join_without_spaces",
    },
    Binding {
        keys: "t",
        command: Command::Go(GoCommand::Tab(true)),
        label: "vim.next_tab",
    },
    Binding {
        keys: "T",
        command: Command::Go(GoCommand::Tab(false)),
        label: "vim.previous_tab",
    },
    Binding {
        keys: "f",
        command: Command::Go(GoCommand::File),
        label: "git.open_in_editor",
    },
    Binding {
        keys: "D",
        command: Command::Go(GoCommand::Docs),
        label: "tabs.open_in_docs",
    },
];

impl HintKind {
    fn bindings(self) -> &'static [Binding] {
        match self {
            Self::Leader => LEADER_BINDINGS,
            Self::Go(_) => GO_BINDINGS,
        }
    }

    fn display(self, prefix: &str, space: &str) -> String {
        display_prefix(
            prefix,
            space,
            match self {
                Self::Leader => space,
                Self::Go(_) => "g",
            },
        )
    }

    fn selector(self) -> &'static str {
        match self {
            Self::Leader => "vim-leader",
            Self::Go(_) => "vim-go",
        }
    }
}

fn group_label(key: char) -> &'static str {
    match key {
        'b' => "vim.buffers",
        'c' => "vim.code",
        'f' => "locator.files",
        'g' => "vim.git_group",
        's' => "vim.search",
        _ => unreachable!("leader groups are defined by the binding table"),
    }
}

fn visible_suffix(text: &str, capacity: usize) -> String {
    let length = text.chars().count();
    if length <= capacity {
        return text.to_owned();
    }
    std::iter::once('…')
        .chain(text.chars().skip(length - capacity + 1))
        .collect()
}

fn display_prefix(prefix: &str, space: &str, first: &str) -> String {
    let keys = prefix.chars().map(|key| {
        if key == ' ' {
            space.to_owned()
        } else {
            key.to_string()
        }
    });
    std::iter::once(first.to_owned())
        .chain(keys)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn plain_key(key: &Keystroke) -> Option<char> {
    let Modifiers {
        alt,
        control,
        platform,
        function,
        shift,
    } = key.modifiers;
    if alt || control || platform || function {
        return None;
    }
    if key.key == "space" {
        return (!shift).then_some(' ');
    }
    let character = |text: &str| {
        let mut chars = text.chars();
        chars.next().filter(|_| chars.next().is_none())
    };
    key.key_char
        .as_deref()
        .and_then(character)
        .or_else(|| character(&key.key))
        .map(|value| {
            if shift {
                value.to_ascii_uppercase()
            } else {
                value
            }
        })
}

fn consume(window: &mut Window, cx: &mut App) {
    window.prevent_default();
    cx.stop_propagation();
}

impl Workspace {
    pub(super) fn intercept_vim_keys(window: &Window, cx: &mut Context<Self>) -> Subscription {
        let workspace = cx.weak_entity();
        let owner = window.window_handle().window_id();
        cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() == owner
                && !super::ui_scale::is_shortcut(&event.keystroke)
            {
                if let Ok(Some(panel)) = workspace.update(cx, |this, cx| {
                        this.vim_intercept(&event.keystroke, window, cx);
                        (!window.default_prevented()
                            && this.view == WorkspaceView::Git
                            && this.config.current().keyboard.vim_mode
                            && this.vim.hint.is_none()
                            && !(plain_key(&event.keystroke) == Some('g')
                                && this.vim_go_scope(window, cx) == Some(GoScope::Diff))
                            && !this.locator.open
                            && !this.repository_overlay_open()
                            && !this.confirming_close
                            && this.image_preview.is_none()
                            && !window.has_active_dialog(cx))
                        .then(|| this.git.clone())
                        .flatten()
                    })
                    // The projection validates edits against the workspace's live buffers.
                    // Release the workspace update before invoking the diff editor.
                    && panel.update(cx, |panel, cx| {
                        panel.vim_diff_key(&event.keystroke, window, cx)
                    })
                {
                    consume(window, cx);
                }
            }
        })
    }

    fn vim_intercept(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        if !self.config.current().keyboard.vim_mode
            || self.locator.open
            || self.repository_overlay_open()
            || self.confirming_close
            || self.image_preview.is_some()
            || window.has_active_dialog(cx)
            || window.has_active_prompt()
        {
            return;
        }
        if self.advance_preview_command(key, window, cx) {
            consume(window, cx);
            return;
        }
        if !key.modifiers.modified() && matches!(key.key.as_str(), "enter" | "escape") {
            let commit = key.key == "enter";
            if self.finish_vim_find(commit, window, cx)
                || self.vim_view_reader().is_some_and(|reader| {
                    reader.update(cx, |reader, cx| reader.finish_vim_find(commit, window, cx))
                })
            {
                consume(window, cx);
                return;
            }
        }
        if self.vim_focus_pane(key, window, cx) {
            self.cancel_vim_keys(cx);
            consume(window, cx);
            return;
        }
        if self
            .vim
            .hint
            .as_ref()
            .is_some_and(|hint| hint.focus.is_focused(window))
            && plain_key(key).is_none()
            && key.key != "escape"
            && !key.is_modifier_key()
        {
            self.cancel_vim_keys(cx);
            consume(window, cx);
            return;
        }
        if key.modifiers
            == (Modifiers {
                control: true,
                ..Modifiers::none()
            })
            && matches!(key.key.as_str(), "o" | "i")
            && self.vim_eligible(window, cx)
            && self.vim_source(window, cx).is_none_or(|file| {
                let editor = file.editor.read(cx);
                file.vim.mode() == source::Mode::Normal
                    && !editor.completion_menu_state().open
                    && !editor.code_action_menu_state().open
            })
        {
            self.navigate_source(key.key == "i", window, cx);
            consume(window, cx);
            return;
        }
        let control = key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform;
        let special = !key.modifiers.modified()
            && matches!(
                key.key.as_str(),
                "enter"
                    | "backspace"
                    | "delete"
                    | "tab"
                    | "left"
                    | "right"
                    | "up"
                    | "down"
                    | "home"
                    | "end"
                    | "pageup"
                    | "pagedown"
            );
        if (special || control && matches!(key.key.as_str(), "v" | "d" | "u" | "b" | "f" | "r"))
            && let Some(file) = self.vim_source(window, cx)
        {
            let path = file.path.clone();
            let locked = self.file_editor_locked(file);
            let file = self.files.get_mut(&path).unwrap();
            let editor = file.editor.read(cx);
            if file.vim.mode() == source::Mode::Insert
                || editor.is_composing()
                || editor.completion_menu_state().open
                || editor.code_action_menu_state().open
            {
                return;
            }
            if let Some(action) =
                file.vim
                    .key_down(&file.editor, locked, key, file.wrap, window, cx)
            {
                self.vim_source_action(action, window, cx);
                cx.notify();
                consume(window, cx);
            } else if special {
                consume(window, cx);
            }
        }
    }

    pub(super) fn reset_vim(&mut self, cx: &mut Context<Self>) {
        self.vim = WorkspaceState::default();
        if let Some(panel) = &self.git {
            panel.update(cx, |panel, cx| {
                panel.set_diff_vim(self.config.current().keyboard.vim_mode, cx);
            });
        }
        for file in self.files.values_mut() {
            file.vim.reset(&file.editor, cx);
            file.editor.update(cx, |editor, cx| {
                editor.set_modal_editing(self.config.current().keyboard.vim_mode, cx)
            });
        }
        cx.notify();
    }

    pub(super) fn cancel_vim_keys(&mut self, cx: &mut Context<Self>) {
        let mut changed = self.vim.hint.take().is_some();
        changed |= std::mem::take(&mut self.vim.reader_prefix);
        changed |= self.vim.preview_command.take().is_some();
        if let Some(file) = self.tabs.active().and_then(|path| self.files.get_mut(path)) {
            changed |= file.vim.clear_pending();
        }
        if self.view == WorkspaceView::Git
            && let Some(panel) = &self.git
        {
            changed |= panel.update(cx, |panel, _| panel.clear_diff_vim_keys());
        }
        if changed {
            cx.notify();
        }
    }

    fn vim_source(&self, window: &Window, cx: &App) -> Option<&OpenFile> {
        self.active_file().filter(|file| {
            self.view == WorkspaceView::Editor
                && !self.terminal_maximized()
                && self.active_editor_preview_mode() != crate::session::EditorPreviewMode::Full
                && file.editor.read(cx).focus_handle(cx).is_focused(window)
        })
    }

    fn vim_view_reader(&self) -> Option<&Entity<crate::docs::DocReader>> {
        match self.view {
            WorkspaceView::Docs => self.doc.as_ref(),
            WorkspaceView::Editor
                if self.active_editor_preview_mode()
                    != crate::session::EditorPreviewMode::Source =>
            {
                self.active_markdown_preview()
            }
            WorkspaceView::Editor | WorkspaceView::Git => None,
        }
    }

    fn vim_reader(&self, window: &Window, cx: &App) -> Option<Entity<crate::docs::DocReader>> {
        self.vim_view_reader()
            .filter(|reader| reader.read(cx).content_focused(window, cx))
            .cloned()
    }

    fn vim_image_focused(&self, window: &Window) -> bool {
        self.view == WorkspaceView::Editor && self.image_viewer_focus.is_focused(window)
    }

    fn vim_eligible(&self, window: &Window, cx: &App) -> bool {
        self.focus.is_focused(window)
            || (self.tree_focus.is_focused(window) && self.tree_edit.is_none())
            || self.vim_reader(window, cx).is_some()
            || self.vim_image_focused(window)
            || self.vim_source(window, cx).is_some_and(|file| {
                file.vim.accepts_leader() && !file.editor.read(cx).is_composing()
            })
            || (self.view == WorkspaceView::Git
                && self
                    .git
                    .as_ref()
                    .is_some_and(|panel| panel.read(cx).vim_focused(window, cx)))
    }

    pub(super) fn vim_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.default_prevented() {
            return;
        }
        if self.locator.open
            || self.repository_overlay_open()
            || self.confirming_close
            || self.image_preview.is_some()
        {
            self.cancel_vim_keys(cx);
            return;
        }
        let key = &event.keystroke;
        let plain = plain_key(key);
        if !self.config.current().keyboard.vim_mode {
            if self.view == WorkspaceView::Docs
                && let Some(reader) = self.vim_reader(window, cx)
            {
                self.vim_reader_key(reader, key, plain, window, cx);
            }
            return;
        }
        if window.has_active_dialog(cx) || window.has_active_prompt() {
            self.cancel_vim_keys(cx);
            return;
        }
        if key.key == "escape" && !key.modifiers.modified() {
            self.vim_escape(window, cx);
            return;
        }
        if self.terminal_focused(window, cx) {
            if ctrl_backslash(key) && !self.terminal_composing(cx) {
                if !event.is_held {
                    self.toggle_terminal_shortcut(&super::ToggleTerminal, window, cx);
                }
                consume(window, cx);
            }
            return;
        }
        if !self
            .vim
            .hint
            .as_ref()
            .is_none_or(|hint| hint.focus.is_focused(window))
        {
            self.cancel_vim_keys(cx);
        }
        if self.vim.hint.is_some() {
            if !event.is_held {
                self.advance_vim_hint(key, plain, window, cx);
            }
            consume(window, cx);
            return;
        }
        if self.vim_eligible(window, cx) {
            if plain == Some(' ') {
                if !event.is_held {
                    self.start_vim_hint(HintKind::Leader, window, cx);
                }
                consume(window, cx);
                return;
            }
            if ctrl_backslash(key) && self.repository.is_some() {
                if !event.is_held {
                    self.toggle_terminal_shortcut(&super::ToggleTerminal, window, cx);
                }
                consume(window, cx);
                return;
            }
        }
        if plain == Some('g')
            && let Some(scope) = self.vim_go_scope(window, cx)
        {
            if !event.is_held {
                self.start_vim_hint(HintKind::Go(scope), window, cx);
            }
            consume(window, cx);
            return;
        }
        if self.vim_tree_key(plain, window, cx) {
            return;
        }
        if let Some(file) = self.vim_source(window, cx) {
            let origin = if file.vim.mode() == source::Mode::Normal
                && matches!(plain, Some('*' | '#'))
                && file.vim.pending().chars().all(|key| key.is_ascii_digit())
            {
                self.source_location(cx)
            } else {
                None
            };
            let path = file.path.clone();
            let locked = self.file_editor_locked(file);
            let file = self.files.get_mut(&path).unwrap();
            let action = file
                .vim
                .key_down(&file.editor, locked, key, file.wrap, window, cx);
            if let Some(action) = action {
                self.vim_source_action(action, window, cx);
                if origin.is_some() {
                    self.record_source_jump(origin, cx);
                }
                consume(window, cx);
                cx.notify();
            }
            return;
        }
        let reader = self.vim_reader(window, cx);
        if self.view == WorkspaceView::Editor
            && (reader.is_some() || self.vim_image_focused(window))
            && self.vim_preview_key(plain, window, cx)
        {
            consume(window, cx);
            return;
        }
        if let Some(reader) = reader {
            self.vim_reader_key(reader, key, plain, window, cx);
        } else if self.view == WorkspaceView::Git
            && let Some(panel) = self.git.clone()
            && panel.update(cx, |panel, cx| panel.vim_key(event, plain, window, cx))
        {
            consume(window, cx);
        }
    }

    fn vim_preview_key(
        &mut self,
        key: Option<char>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.tabs.active().is_none() {
            return false;
        }
        match key {
            Some('H' | 'L') => self.cycle_tabs(key == Some('L'), window, cx),
            Some('X') => self.vim_source_action(source::Action::ClosePrevious, window, cx),
            Some(':') => {
                let Some(focus) = window.focused(cx) else {
                    return false;
                };
                let focus_out = cx.on_focus_out(&focus, window, |this, _, _, _| {
                    this.vim.preview_command = None;
                });
                self.vim.preview_command = Some(PreviewCommand {
                    pending: ":".into(),
                    _focus_out: focus_out,
                });
            }
            _ => return false,
        }
        true
    }

    fn advance_preview_command(
        &mut self,
        key: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(command) = self.vim.preview_command.as_mut() else {
            return false;
        };
        if key.is_modifier_key() {
            return false;
        }
        if key.key == "escape" && !key.modifiers.modified() {
            self.vim.preview_command = None;
            return true;
        }
        let plain = plain_key(key);
        if plain.is_none()
            && (key.modifiers.modified() || !matches!(key.key.as_str(), "enter" | "backspace"))
        {
            self.vim.preview_command = None;
            return false;
        }
        if let Some(action) = source::advance_file_command(&mut command.pending, key, plain) {
            self.vim.preview_command = None;
            if !matches!(action, source::Action::Save | source::Action::SaveAndClose)
                || self.active_file().is_some()
            {
                self.vim_source_action(action, window, cx);
            }
        } else if command.pending.is_empty() {
            self.vim.preview_command = None;
        }
        true
    }

    fn vim_reader_key(
        &mut self,
        reader: Entity<crate::docs::DocReader>,
        key: &Keystroke,
        plain: Option<char>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let top = plain == Some('g') && self.vim.reader_prefix;
        self.vim.reader_prefix = plain == Some('g') && !top;
        let modal = self.config.current().keyboard.vim_mode;
        if self.vim.reader_prefix
            || reader.update(cx, |reader, cx| {
                reader.vim_key(key, plain, top, modal, window, cx)
            })
        {
            consume(window, cx);
        }
    }

    pub(super) fn vim_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.config.current().keyboard.vim_mode
            || self.locator.open
            || self.repository_overlay_open()
        {
            return;
        }
        let (path, dismiss_hover) = if let Some(file) = self.vim_source(window, cx) {
            let editor = file.editor.read(cx);
            let hover = editor.has_language_hover();
            if editor.is_composing()
                || editor.completion_menu_state().open
                || editor.code_action_menu_state().open
                || (hover && file.vim.mode() != source::Mode::Insert)
            {
                return;
            }
            (Some(file.path.clone()), hover)
        } else {
            (None, false)
        };
        if self
            .vim
            .hint
            .as_ref()
            .is_some_and(|hint| hint.focus.is_focused(window))
        {
            self.cancel_vim_keys(cx);
            consume(window, cx);
        } else if !dismiss_hover && path.is_some() && self.dismiss_active_signature(cx) {
            consume(window, cx);
        } else if let Some(path) = path
            && let Some(file) = self.files.get_mut(&path)
        {
            if dismiss_hover {
                file.editor.update(cx, |editor, cx| {
                    EditorMode::dismiss_hover(editor, window, cx);
                });
            }
            if file.vim.escape(&file.editor, window, cx) {
                consume(window, cx);
                cx.notify();
            }
        }
    }

    fn vim_tree_key(
        &mut self,
        key: Option<char>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.tree_focus.is_focused(window) || self.tree_edit.is_some() {
            return false;
        }
        match key {
            Some('h') => self.navigate_tree(NavigateTree::Left, window, cx),
            Some('j') => self.navigate_tree(NavigateTree::Down, window, cx),
            Some('k') => self.navigate_tree(NavigateTree::Up, window, cx),
            Some('l') => self.navigate_tree(NavigateTree::Right, window, cx),
            Some('p') => self.open_tree_file(true, window, cx),
            _ => return false,
        }
        consume(window, cx);
        true
    }

    fn start_vim_hint(&mut self, kind: HintKind, window: &mut Window, cx: &mut Context<Self>) {
        if !kind
            .bindings()
            .iter()
            .any(|binding| self.vim_hint_command_available(kind, binding.command, window, cx))
        {
            return;
        }
        let Some(focus) = window.focused(cx) else {
            return;
        };
        self.vim.reader_prefix = false;
        let focus_out = cx.on_focus_out(&focus, window, |this, _, _, cx| this.cancel_vim_keys(cx));
        self.vim.hint = Some(KeyHint {
            kind,
            focus,
            prefix: String::new(),
            visible: false,
            _focus_out: focus_out,
            timer: Task::ready(()),
        });
        self.schedule_vim_hint(window, cx);
        cx.notify();
    }

    fn schedule_vim_hint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hint) = &mut self.vim.hint else {
            return;
        };
        if !hint.visible {
            let timer = cx.background_executor().timer(HINT_DELAY);
            hint.timer = cx.spawn_in(window, async move |this, cx| {
                timer.await;
                _ = this.update_in(cx, |this, window, cx| {
                    if let Some(hint) = &mut this.vim.hint
                        && hint.focus.is_focused(window)
                    {
                        hint.visible = true;
                        cx.notify();
                    }
                });
            });
        }
    }

    fn advance_vim_hint(
        &mut self,
        stroke: &Keystroke,
        key: Option<char>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut hint = self.vim.hint.take().unwrap();
        if let Some(key) = key {
            hint.prefix.push(key);
            if let Some(binding) = hint.kind.bindings().iter().find(|binding| {
                binding.keys.starts_with(&hint.prefix)
                    && self.vim_hint_command_available(hint.kind, binding.command, window, cx)
            }) {
                if binding.keys == hint.prefix {
                    match (hint.kind, binding.command) {
                        (HintKind::Go(scope), Command::Go(command)) => {
                            self.run_vim_go(scope, command, stroke, window, cx);
                        }
                        _ => self.run_vim_command(binding.command, window, cx),
                    }
                } else {
                    self.vim.hint = Some(hint);
                    self.schedule_vim_hint(window, cx);
                }
            }
        }
        cx.notify();
    }

    fn vim_hint_command_available(
        &self,
        kind: HintKind,
        command: Command,
        window: &Window,
        cx: &App,
    ) -> bool {
        match (kind, command) {
            (HintKind::Go(scope), Command::Go(command)) => {
                self.vim_go_scope(window, cx) == Some(scope)
                    && self.vim_go_available(scope, command, window, cx)
            }
            _ => self.vim_command_available(command, window, cx),
        }
    }

    fn vim_command_available(&self, command: Command, window: &Window, cx: &App) -> bool {
        let source = self.vim_source(window, cx);
        match command {
            Command::DocumentSymbols | Command::WorkspaceSymbols => {
                self.locator_symbols_available(command == Command::WorkspaceSymbols, cx)
            }
            Command::Close => self.view == WorkspaceView::Editor && self.tabs.active().is_some(),
            Command::Comment => source.is_some_and(|file| !self.file_editor_locked(file)),
            Command::CodeActions => {
                source.is_some_and(|file| file.editor.read(cx).can_open_code_actions())
            }
            Command::Rename => source.is_some_and(|file| {
                file.editor
                    .read(cx)
                    .context_menu_capabilities()
                    .has_rename()
            }),
            Command::Format => source.is_some_and(|file| {
                !self.file_editor_locked(file) && file.language_server.is_some()
            }),
            Command::Diff => {
                self.repository.is_some()
                    && self
                        .git
                        .as_ref()
                        .is_none_or(|panel| panel.read(cx).vim_has_worktree_changes())
            }
            Command::Tree if self.view == WorkspaceView::Git => self
                .git
                .as_ref()
                .is_some_and(|panel| panel.read(cx).vim_has_tree()),
            _ => self.repository.is_some(),
        }
    }

    fn run_vim_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::Files => self.open_locator(window, cx),
            Command::Text => self.open_text_locator(window, cx),
            Command::DocumentSymbols | Command::WorkspaceSymbols => {
                self.open_symbol_locator(command == Command::WorkspaceSymbols, window, cx);
            }
            Command::View(view) => {
                self.switch_view_shortcut(&super::SwitchWorkspaceView(view), window, cx);
            }
            Command::Tree if self.view != WorkspaceView::Git => {
                if self.tree_focus.is_focused(window) {
                    if let Some(focus) = self.vim.return_from_tree.take() {
                        focus.focus(window, cx);
                    } else if self.view == WorkspaceView::Docs {
                        self.focus_document_content(window, cx);
                    } else {
                        self.focus_workspace_content(None, window, cx);
                    }
                } else {
                    self.focus_vim_tree(window, cx);
                }
            }
            Command::Terminal => self.toggle_terminal_shortcut(&super::ToggleTerminal, window, cx),
            Command::Git | Command::Tree | Command::Status | Command::Diff | Command::Log => {
                let active = self
                    .tabs
                    .active()
                    .map(|path| path.to_string_lossy().into_owned());
                self.switch_view(WorkspaceView::Git, window, cx);
                if let Some(panel) = self.git.clone() {
                    panel.update(cx, |panel, cx| match command {
                        Command::Tree => panel.vim_toggle_tree(window, cx),
                        Command::Status => panel.vim_worktree(false, active, window, cx),
                        Command::Diff => panel.vim_worktree(true, active, window, cx),
                        Command::Log => panel.vim_history(window, cx),
                        _ => {}
                    });
                }
            }
            Command::Close => self.close_active_tab(&super::CloseTab, window, cx),
            Command::Comment => self.vim_source_action(source::Action::Comment, window, cx),
            Command::CodeActions => window.dispatch_action(Box::new(input::ToggleCodeActions), cx),
            Command::Rename => window.dispatch_action(Box::new(gpui_base::input::RenameSymbol), cx),
            Command::Format => self.format_document(&super::FormatDocument, window, cx),
            Command::Go(_) => unreachable!("g commands are dispatched by scope"),
        }
    }

    pub(super) fn vim_overlay(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let hint = self.vim.hint.as_ref()?;
        if !hint.visible || !hint.focus.is_focused(window) {
            return None;
        }
        let mut items = Vec::new();
        for binding in hint.kind.bindings() {
            let Some(suffix) = binding.keys.strip_prefix(&hint.prefix) else {
                continue;
            };
            let Some(key) = suffix.chars().next() else {
                continue;
            };
            if !self.vim_hint_command_available(hint.kind, binding.command, window, cx)
                || items.iter().any(|(existing, _)| *existing == key)
            {
                continue;
            }
            items.push((
                key,
                match hint.kind {
                    HintKind::Leader if suffix.len() > key.len_utf8() => group_label(key),
                    _ => binding.label,
                },
            ));
        }
        if items.is_empty() {
            return None;
        }
        let selector = hint.kind.selector();
        let panel = v_flex()
            .id(selector)
            .occlude()
            .debug_selector(move || selector.into())
            .popover_style(cx)
            .w(px(320.).min(window.viewport_size().width - px(24.)))
            .max_h((window.viewport_size().height * 0.75).max(px(60.)))
            .overflow_y_scroll()
            .gap_1()
            .px_3()
            .py_2()
            .text_size(px(super::text_base_size(cx)))
            .child(
                div()
                    .font_semibold()
                    .child(hint.kind.display(&hint.prefix, "Space")),
            )
            .children(items.into_iter().map(|(key, label)| {
                let key = if key == ' ' {
                    "Space".to_owned()
                } else {
                    key.to_string()
                };
                h_flex()
                    .debug_selector(|| format!("{selector}-key-{key}"))
                    .gap_2()
                    .child(div().w(px(52.)).child(key))
                    .child(t!(label).into_owned())
            }));
        let bottom = if self.status_file().is_some() {
            8. + STATUS_HEIGHT
        } else {
            8.
        };
        Some(
            deferred(
                div()
                    .absolute()
                    .right(px(12.))
                    .bottom(px(bottom))
                    .child(panel),
            )
            .with_priority(gpui_base::POPUP_PRIORITY - 1),
        )
    }

    fn focus_vim_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.vim.return_from_tree = window.focused(cx);
        self.set_file_tree_visible(true);
        let path = if self.view == WorkspaceView::Docs {
            self.doc
                .as_ref()
                .map(|doc| doc.read(cx).path.clone())
                .or_else(|| self.session.document.clone())
        } else {
            self.tabs.active().map(std::path::Path::to_owned)
        }
        .filter(|path| !path.is_absolute());
        if let Some(path) = path {
            if self.view == WorkspaceView::Docs {
                self.docs_tree_reveal = Some(path.clone());
            } else {
                self.tree_selected = Some(path.clone());
                self.tree_reveal = Some(path.clone());
            }
            self.focus_tree(path, window, cx);
        } else {
            self.tree_focus.focus(window, cx);
            cx.notify();
        }
        self.resume_tree_reveal(window, cx);
    }

    fn vim_focus_left_or_right(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.view == WorkspaceView::Git {
            return self.git.as_ref().is_some_and(|panel| {
                panel.update(cx, |panel, cx| {
                    panel.vim_focus_adjacent_pane(key, window, cx)
                })
            });
        }
        let in_tree = self.tree_focus.is_focused(window) && self.tree_edit.is_none();
        let in_source = self.vim_source(window, cx).is_some();
        let reader = self.vim_reader(window, cx);
        let preview = self.view == WorkspaceView::Editor
            && self.active_editor_preview_mode() == crate::session::EditorPreviewMode::Split;
        let svg_preview = preview
            && self.tabs.active().is_some_and(|path| {
                super::editor_preview::PreviewKind::for_path(path)
                    == Some(super::editor_preview::PreviewKind::Svg)
            });
        let in_image = self.vim_image_focused(window);
        let in_content = in_source || reader.is_some() || in_image || self.focus.is_focused(window);
        match key {
            "h" if preview && (reader.is_some() || svg_preview && in_image) => {
                self.focus_workspace_content(None, window, cx);
            }
            "h" if in_content
                && !in_tree
                && self.standalone.is_none()
                && self.repository.is_some() =>
            {
                self.focus_vim_tree(window, cx);
            }
            "l" if in_tree => {
                self.vim.return_from_tree = None;
                if self.view == WorkspaceView::Docs {
                    self.focus_document_content(window, cx);
                } else {
                    self.focus_workspace_content(None, window, cx);
                }
            }
            "l" if in_source && preview => {
                if let Some(preview) = self.active_markdown_preview() {
                    preview.update(cx, |preview, cx| preview.focus_content(window, cx));
                } else if svg_preview {
                    self.image_viewer_focus.focus(window, cx);
                } else {
                    return false;
                }
            }
            _ => return false,
        }
        true
    }

    fn vim_focus_pane(
        &mut self,
        key: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if key.modifiers
            != (Modifiers {
                control: true,
                ..Modifiers::default()
            })
            || !matches!(key.key.as_str(), "h" | "j" | "k" | "l")
            || self.terminal_maximized()
        {
            return false;
        }
        if self.terminal_focused(window, cx) {
            return key.key == "k"
                && !self.terminal_composing(cx)
                && self.focus_above_terminal(window, cx);
        }
        let source = self.vim_source(window, cx);
        if source.is_some_and(|file| {
            let editor = file.editor.read(cx);
            editor.is_composing()
                || editor.completion_menu_state().open
                || editor.code_action_menu_state().open
        }) {
            return false;
        }
        let diff = self
            .git
            .as_ref()
            .filter(|_| self.view == WorkspaceView::Git);
        if diff.is_some_and(|panel| panel.read(cx).vim_diff_composing(window, cx)) {
            return false;
        }
        let command_mode = source.is_some_and(|file| file.vim.mode() != source::Mode::Insert)
            || diff.is_some_and(|panel| panel.read(cx).vim_diff_command_mode(window, cx));
        let moved = match key.key.as_str() {
            "h" | "l" => self.vim_focus_left_or_right(&key.key, window, cx),
            "j" if self.view == WorkspaceView::Editor
                && (source.is_some()
                    || self.vim_reader(window, cx).is_some()
                    || self.vim_image_focused(window)) =>
            {
                self.focus_open_terminal(window, cx)
            }
            _ => false,
        };
        moved || command_mode
    }

    fn status_vim_button(&self, colors: Palette, cx: &Context<Self>) -> Div {
        let enabled = self.config.current().keyboard.vim_mode;
        let (art, label, color, selector) = if enabled {
            (
                include_bytes!("icons/vim-on.svg").as_slice(),
                t!("vim.disable"),
                rgb(if cx.theme().mode.is_dark() {
                    0x9cc4aa
                } else {
                    0x42765a
                })
                .into(),
                "status-vim-icon-on",
            )
        } else {
            (
                include_bytes!("icons/vim-off.svg").as_slice(),
                t!("vim.enable"),
                colors.muted,
                "status-vim-icon-off",
            )
        };
        div()
            .flex_shrink_0()
            .debug_selector(move || selector.into())
            .child(
                super::quiet_button("status-vim-toggle")
                    .with_size(STATUS_BUTTON_SIZE)
                    .size(STATUS_BUTTON_SIZE)
                    .p_0()
                    .icon_size(STATUS_ICON_SIZE)
                    .icon(Icon::default().data(art).text_color(color))
                    .toggled(enabled)
                    .accessibility_label(label.clone())
                    .tooltip(label)
                    .debug_selector(|| "status-vim-toggle".into())
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_vim_mode(window, cx))),
            )
    }

    pub(super) fn vim_status(
        &self,
        file: &OpenFile,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
        size: f32,
    ) -> Div {
        let button = self.status_vim_button(colors, cx);
        let group = h_flex().h_full().gap(STATUS_ITEM_GAP).flex_shrink_0();
        if !self.config.current().keyboard.vim_mode {
            return group.pl(STATUS_ITEM_GAP).child(button);
        }

        let mode = file.vim.mode();
        let label = mode.label();
        let (background, foreground) = mode_colors(mode, cx.theme().mode.is_dark());
        let pending = if let Some(hint) = &self.vim.hint {
            hint.kind.display(&hint.prefix, SPACE)
        } else if self.vim_source(window, cx).is_some() {
            file.vim.pending().to_owned()
        } else {
            String::new()
        };
        let mono = cx.theme().mono_font_family.clone();
        let mode_width = mode_label_width(window, &mono, size, label);
        let mode_slot = max_mode_width(window, &mono, size);
        let capacity = if self.vim.wide_status { 9 } else { 5 };
        let pending_width = status_text_width(
            window,
            &mono,
            size,
            &visible_suffix(PENDING_SAMPLE, capacity),
            FontWeight::NORMAL,
        );
        let pending_display = visible_suffix(&pending, capacity);
        group
            .font_family(mono)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .w(mode_width)
                    .h(MODE_HEIGHT)
                    .justify_center()
                    .font_semibold()
                    .bg(background)
                    .text_color(foreground)
                    .child(label)
                    .debug_selector(|| format!("status-vim-mode: {label}")),
            )
            .child(button)
            .child(
                baseline_aligned(cx, size, size, pending_display.clone())
                    .id("status-vim-pending")
                    .w(pending_width + mode_slot - mode_width)
                    .overflow_hidden()
                    .aria_label(pending)
                    .debug_selector(|| format!("status-vim-pending: {pending_display}")),
            )
    }

    pub(super) fn measure_vim_status(
        &self,
        cx: &Context<Self>,
        size: f32,
    ) -> Option<impl IntoElement + use<>> {
        if !self.config.current().keyboard.vim_mode {
            return None;
        }
        let workspace = cx.weak_entity();
        let wide = self.vim.wide_status;
        let family = cx.theme().mono_font_family.clone();
        Some(
            canvas(
                move |bounds, window, _| {
                    let group = max_mode_width(window, &family, size)
                        + STATUS_BUTTON_SIZE
                        + STATUS_ITEM_GAP * 2.
                        + status_text_width(
                            window,
                            &family,
                            size,
                            PENDING_SAMPLE,
                            FontWeight::NORMAL,
                        );
                    bounds.size.width >= group + px(12. * size + 12.)
                },
                move |_, next_wide, window, cx| {
                    if next_wide != wide {
                        let workspace = workspace.clone();
                        window.defer(cx, move |_, cx| {
                            _ = workspace.update(cx, |this, cx| {
                                if this.vim.wide_status != next_wide {
                                    this.vim.wide_status = next_wide;
                                    cx.notify();
                                }
                            });
                        });
                    }
                },
            )
            .absolute()
            .size_full(),
        )
    }
}

fn mode_colors(mode: source::Mode, dark: bool) -> (Rgba, Rgba) {
    let (background, foreground) = match (mode, dark) {
        (source::Mode::Normal, false) => (0xe8edff, 0x354fb0),
        (source::Mode::Normal, true) => (0x363940, 0xe1e4ea),
        (source::Mode::Insert, false) => (0xe1f2e8, 0x0b6747),
        (source::Mode::Insert, true) => (0x2b4239, 0xc0e1cf),
        (_, false) => (0xf1e6fc, 0x7225b9),
        (_, true) => (0x41374d, 0xe2d3ef),
    };
    (rgb(background), rgb(foreground))
}

fn status_text_width(
    window: &Window,
    family: &SharedString,
    size: f32,
    text: &str,
    weight: FontWeight,
) -> Pixels {
    let mut run = TextStyle::default().to_run(text.len());
    run.font.family = family.clone();
    run.font.weight = weight;
    window
        .text_system()
        .layout_line(text, px(size), &[run], None)
        .width
}

fn mode_label_width(window: &Window, family: &SharedString, size: f32, label: &str) -> Pixels {
    let width = |value| status_text_width(window, family, size, value, FontWeight::SEMIBOLD);
    (width(label) + width("0") * 2.).ceil()
}

fn max_mode_width(window: &Window, family: &SharedString, size: f32) -> Pixels {
    source::Mode::ALL
        .into_iter()
        .map(|mode| mode_label_width(window, family, size, mode.label()))
        .fold(px(0.), Pixels::max)
}

fn ctrl_backslash(key: &Keystroke) -> bool {
    key.modifiers
        == Modifiers {
            control: true,
            ..Default::default()
        }
        && matches!(key.key.as_str(), "\\" | "backslash")
}

#[cfg(test)]
mod tests;
