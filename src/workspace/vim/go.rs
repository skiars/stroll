use gpui::*;

use super::{GoCommand, GoScope, Workspace};
use crate::workspace::{SourcePosition, WorkspaceView};

impl Workspace {
    pub(super) fn vim_go_scope(&self, window: &Window, cx: &App) -> Option<GoScope> {
        if let Some(file) = self.vim_source(window, cx) {
            let editor = file.editor.read(cx);
            return (file.vim.accepts_leader()
                && !editor.is_composing()
                && !editor.completion_menu_state().open
                && !editor.code_action_menu_state().open)
                .then_some(GoScope::Source);
        }
        if self.vim_reader(window, cx).is_some() {
            return Some(GoScope::Reader);
        }
        if self.vim_image_focused(window) && self.tabs.paths().len() > 1 {
            return Some(GoScope::Image);
        }
        if self.view == WorkspaceView::Git
            && let Some(panel) = &self.git
        {
            let panel = panel.read(cx);
            if panel.vim_diff_accepts_leader(window, cx) {
                return Some(GoScope::Diff);
            }
            if panel.vim_changed_file(window, cx).is_some() {
                return Some(GoScope::ChangedFiles);
            }
        }
        None
    }

    pub(super) fn vim_go_available(
        &self,
        scope: GoScope,
        command: GoCommand,
        window: &Window,
        cx: &App,
    ) -> bool {
        match (scope, command) {
            (GoScope::Source, GoCommand::Top)
            | (GoScope::Reader, GoCommand::Top | GoCommand::File)
            | (GoScope::Diff, GoCommand::Top)
            | (GoScope::ChangedFiles, GoCommand::File) => true,
            (GoScope::Source | GoScope::Reader | GoScope::Image, GoCommand::Tab(_)) => {
                self.view == WorkspaceView::Editor && self.tabs.paths().len() > 1
            }
            (GoScope::Source, command) => self.vim_source(window, cx).is_some_and(|file| {
                let editor = file.editor.read(cx);
                match command {
                    GoCommand::Definition => editor.context_menu_capabilities().has_definition(),
                    GoCommand::References => {
                        !file.missing
                            && file
                                .language_server
                                .as_ref()
                                .is_some_and(|server| server.document.supports_references())
                    }
                    GoCommand::Comment | GoCommand::Join => {
                        !self.file_editor_locked(file) && editor.is_editable()
                    }
                    _ => false,
                }
            }),
            (GoScope::Diff, GoCommand::Definition | GoCommand::Join) => {
                self.git.as_ref().is_some_and(|panel| {
                    panel
                        .read(cx)
                        .vim_diff_go_available(command == GoCommand::Join, window, cx)
                })
            }
            (GoScope::Diff, GoCommand::File) => self
                .git
                .as_ref()
                .is_some_and(|panel| panel.read(cx).vim_diff_has_file(window, cx)),
            (GoScope::ChangedFiles, GoCommand::Docs) => self.git.as_ref().is_some_and(|panel| {
                panel
                    .read(cx)
                    .vim_changed_file(window, cx)
                    .is_some_and(|path| crate::docs::is_markdown(std::path::Path::new(path)))
            }),
            _ => false,
        }
    }

    pub(super) fn run_vim_go(
        &mut self,
        scope: GoScope,
        command: GoCommand,
        stroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match (scope, command) {
            (GoScope::Source | GoScope::Reader | GoScope::Image, GoCommand::Tab(forward)) => {
                self.cycle_tabs(forward, window, cx);
            }
            (GoScope::Reader, GoCommand::Top) => {
                if let Some(reader) = self.vim_reader(window, cx) {
                    reader.update(cx, |reader, cx| {
                        reader.vim_key(stroke, Some('g'), true, true, window, cx);
                    });
                }
            }
            (GoScope::Reader, GoCommand::File) => {
                if let Some(reader) = self.vim_reader(window, cx) {
                    let reader = reader.read(cx);
                    let path = reader.path.clone();
                    let line = reader.session_position(cx).and_then(|position| {
                        let offset = position
                            .line
                            .map(|line| line.source_offset)
                            .or(position.source_offset)?;
                        let source = reader.source_text();
                        let bytes = source.as_bytes().get(..offset)?;
                        u32::try_from(bytes.iter().filter(|&&byte| byte == b'\n').count()).ok()
                    });
                    let from = self
                        .editor_preview_history_location(window, cx)
                        .unwrap_or_else(|| self.locator_history_origin(window, cx));
                    let position =
                        line.map(|line| SourcePosition::Lsp(lsp_types::Position::new(line, 0)));
                    self.open_source_jump(path, position, from, window, cx);
                }
            }
            (GoScope::Diff | GoScope::ChangedFiles, GoCommand::File | GoCommand::Docs) => {
                if let Some(panel) = self.git.clone() {
                    panel.update(cx, |panel, cx| {
                        panel.vim_open_git_file(command == GoCommand::Docs, window, cx);
                    });
                }
            }
            (GoScope::Source, _) => {
                let Some(file) = self.vim_source(window, cx) else {
                    return;
                };
                let path = file.path.clone();
                let locked = self.file_editor_locked(file);
                let file = self.files.get_mut(&path).unwrap();
                let prefix = ["g", "c"];
                let prefix = if command == GoCommand::Comment {
                    &prefix[..]
                } else {
                    &prefix[..1]
                };
                let keys = prefix.iter().map(|key| Keystroke::parse(key).unwrap());
                for key in keys.chain(std::iter::once(stroke.clone())) {
                    if let Some(action) =
                        file.vim
                            .key_down(&file.editor, locked, &key, file.wrap, window, cx)
                        && !matches!(action, super::source::Action::Handled)
                    {
                        self.vim_source_action(action, window, cx);
                        break;
                    }
                }
            }
            (GoScope::Diff, _) => {
                let Some(panel) = self.git.clone() else {
                    return;
                };
                let focus = window.focused(cx);
                let stroke = stroke.clone();
                // Projected edits read the workspace's live buffers.
                window.defer(cx, move |window, cx| {
                    if window.focused(cx) == focus {
                        panel.update(cx, |panel, cx| {
                            panel.vim_diff_key(&Keystroke::parse("g").unwrap(), window, cx);
                            panel.vim_diff_key(&stroke, window, cx);
                        });
                    }
                });
            }
            _ => {}
        }
    }
}
