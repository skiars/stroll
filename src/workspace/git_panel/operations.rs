use std::path::Path;

use gpui_component::{
    WindowExt as _,
    button::DropdownButton,
    input::{InputGroup, InputGroupAddon, InputGroupAddonAlignment, Textarea},
};

use super::*;
use crate::git::{FileAction, HistoryEdit, HistoryTip};

pub(super) struct CommitAmend {
    tip: HistoryTip,
    pub(super) draft: SharedString,
}

pub(super) fn commit_message_editor(state: &Entity<TextareaState>, cx: &App) -> Textarea {
    Textarea::new(state)
        .aria_label(t!("git.commit_message"))
        .text_size(px(text_base_size(cx)))
        .line_height(relative(MESSAGE_LINE_HEIGHT))
}

impl GitPanel {
    pub(super) fn current_worktree(&self) -> bool {
        matches!(self.selection, Some(Selection::Worktree(_))) && !self.foreign_branch()
    }

    pub(super) fn writes_busy(&self, cx: &App) -> bool {
        self.staging
            || self
                .workspace
                .read_with(cx, |workspace, _| {
                    workspace.tree_busy || !workspace.git_edit.is_empty()
                })
                .unwrap_or(true)
    }

    pub(super) fn refresh_commit_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.backend.supports_git_operations() {
            return;
        }
        let backend = self.backend.clone();
        let work = cx.background_spawn(async move {
            backend.commit_state().map_err(|error| error.to_string())
        });
        self.commit_state_task = cx.spawn_in(window, async move |this, cx| {
            let state = work.await;
            _ = this.update_in(cx, |this, _, cx| {
                this.commit_state = Some(state);
                App::notify(cx, this.review_pane.entity_id());
            });
        });
    }

    pub(super) fn replay_worktree_history(
        &self,
        undo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current_worktree() && self.snapshot.is_none() && !self.writes_busy(cx) {
            _ = self.workspace.update(cx, |workspace, cx| {
                workspace.replay_git_history(None, None, undo, window, cx)
            });
        }
    }

    fn commit_unavailable(&self, cx: &App, amend: bool) -> Option<Cow<'static, str>> {
        if !self.backend.supports_git_operations() {
            return Some(t!("git.upgrade_operations"));
        }
        if self.writes_busy(cx) {
            return Some(t!("git.git_busy"));
        }
        match &self.commit_state {
            None => return Some(t!("git.loading_changes")),
            Some(Err(error)) => return Some(error.clone().into()),
            Some(Ok(state)) if state.blocked => return Some(t!("git.commit_blocked")),
            Some(Ok(state)) if !amend && !state.ready => {
                return Some(t!("git.nothing_to_commit"));
            }
            Some(Ok(_)) => {}
        }
        None
    }

    fn commit_disabled_reason(&self, cx: &App) -> Option<Cow<'static, str>> {
        if self.history_preparing {
            return Some(t!("git.loading_changes"));
        }
        let amend = self.commit_amend.as_ref();
        if amend.is_some_and(|amend| self.editable_tip().as_ref() != Some(&amend.tip)) {
            return Some(t!("git.history_edit_stale"));
        }
        self.commit_unavailable(cx, amend.is_some()).or_else(|| {
            (!git::valid_message(&self.commit_message.read(cx).value()))
                .then(|| t!("git.commit_message_required"))
        })
    }

    pub(super) fn clear_commit_amend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(amend) = self.commit_amend.take() {
            self.commit_message
                .update(cx, |input, cx| input.set_value(amend.draft, window, cx));
            cx.notify();
        }
    }

    fn toggle_commit_amend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current_worktree() || self.writes_busy(cx) || self.history_preparing {
            return;
        }
        if self.commit_amend.is_some() {
            self.clear_commit_amend(window, cx);
            return;
        }
        let (Some(tip), Some(Ok(expected))) = (self.editable_tip(), self.commit_state.clone())
        else {
            return;
        };
        self.cancel_ai_commit();
        self.history_preparing = true;
        let backend = self.backend.clone();
        let work = cx.background_spawn(async move {
            let message = backend.amend_message(&tip, &expected);
            (tip, message)
        });
        cx.spawn_in(window, async move |panel, cx| {
            let (tip, message) = work.await;
            _ = panel.update_in(cx, |panel, window, cx| {
                panel.history_preparing = false;
                cx.notify();
                if !panel.current_worktree()
                    || panel.writes_busy(cx)
                    || panel.editable_tip().as_ref() != Some(&tip)
                {
                    return;
                }
                match message {
                    Ok(message) => {
                        let draft = panel.commit_message.read(cx).value();
                        panel.commit_amend = Some(CommitAmend { tip, draft });
                        panel.commit_message.update(cx, |input, cx| {
                            input.set_value(message, window, cx);
                            input.focus_handle(cx).focus(window, cx);
                        });
                    }
                    Err(error) => panel.report_operation_error(error, window, cx),
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn commit_ai(&self, cx: &App) -> Option<crate::ai::Snapshot> {
        self.workspace
            .read_with(cx, |workspace, _| workspace.ai.snapshot(false))
            .ok()
            .flatten()
    }

    fn ai_disabled_reason(&self, cx: &App) -> Option<Cow<'static, str>> {
        self.commit_unavailable(cx, false).or_else(|| {
            if !self.backend.supports_ai_context() {
                Some(t!("ai.upgrade_helper"))
            } else if !self
                .commit_message
                .read(cx)
                .text()
                .chars()
                .all(char::is_whitespace)
            {
                Some(t!("ai.commit_requires_empty"))
            } else {
                None
            }
        })
    }

    pub(crate) fn ai_settings_changed(&mut self, cx: &mut Context<Self>) {
        self.cancel_ai_commit();
        self.editor.clear_hover(cx);
        App::notify(cx, self.review_pane.entity_id());
    }

    pub(super) fn cancel_ai_commit(&mut self) {
        self.ai_commit_pending = false;
        self.ai_commit_task = Task::ready(());
    }

    fn generate_commit_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current_worktree()
            || self.ai_commit_pending
            || self.ai_disabled_reason(cx).is_some()
        {
            return;
        }
        let (Some(Ok(expected)), Some(snapshot)) = (self.commit_state.clone(), self.commit_ai(cx))
        else {
            return;
        };
        let revision = self.commit_message.read(cx).document_revision();
        let backend = self.backend.clone();
        let work = cx.background_spawn({
            let expected = expected.clone();
            let snapshot = snapshot.clone();
            async move { snapshot.commit_message(backend, expected).await }
        });
        self.ai_commit_pending = true;
        self.ai_commit_task = cx.spawn_in(window, async move |panel, cx| {
            let result = work.await;
            _ = panel.update_in(cx, |panel, window, cx| {
                panel.ai_commit_pending = false;
                if panel.current_worktree()
                    && snapshot.is_current()
                    && panel.commit_message.read(cx).document_revision() == revision
                {
                    match result {
                        Ok(message)
                            if panel
                                .commit_state
                                .as_ref()
                                .and_then(|state| state.as_ref().ok())
                                == Some(&expected)
                                && !panel.writes_busy(cx) =>
                        {
                            panel
                                .commit_message
                                .update(cx, |input, cx| input.set_value(message, window, cx));
                        }
                        Ok(_) => {}
                        Err(error) => super::super::toast::error(window, cx, error.to_string()),
                    }
                }
                App::notify(cx, panel.review_pane.entity_id());
            });
        });
        App::notify(cx, self.review_pane.entity_id());
    }

    pub(super) fn commit_controls(&self, cx: &Context<Self>) -> impl IntoElement {
        let amend = self.commit_amend.is_some();
        let reason = self.commit_disabled_reason(cx);
        let ai_enabled = self.commit_ai(cx).is_some();
        let ai_reason = ai_enabled.then(|| self.ai_disabled_reason(cx)).flatten();
        let commit = action_button("git-create-commit")
            .debug_selector(|| "git-create-commit".into())
            .secondary()
            .small()
            .flex_shrink_0()
            .label(if amend {
                t!("git.amend_commit")
            } else {
                t!("git.commit_action")
            })
            .disabled(reason.is_some())
            .when_some(reason, |button, reason| button.tooltip(reason))
            .on_click(cx.listener(|this, _, window, cx| this.submit_commit(window, cx)));
        let commit = if self.backend.supports_history_edit() {
            let owner = cx.weak_entity();
            let amend_unavailable = self.history_preparing
                || if amend {
                    self.writes_busy(cx)
                } else {
                    self.editable_tip().is_none() || self.commit_unavailable(cx, true).is_some()
                };
            let split = DropdownButton::new("git-commit-split")
                .button(commit)
                .dropdown_menu(move |menu, _, _| {
                    let owner = owner.clone();
                    menu.min_w(px(132.)).item_with_check(
                        PopupMenuItem::new(t!("git.amend_commit"))
                            .disabled(amend_unavailable)
                            .on_click(move |_, window, cx| {
                                _ = owner
                                    .update(cx, |panel, cx| panel.toggle_commit_amend(window, cx));
                            }),
                        amend,
                    )
                });
            div()
                .debug_selector(|| "git-commit-split".into())
                .child(split)
                .into_any_element()
        } else {
            commit.into_any_element()
        };
        div()
            .w_full()
            .debug_selector(|| "git-commit-message-entry".into())
            .child(
                InputGroup::new("git-commit-controls")
                    .small()
                    .w_full()
                    .readonly(self.staging || !self.backend.supports_git_operations())
                    .input(commit_message_editor(&self.commit_message, cx).min_h(px(32.)))
                    .addon(
                        InputGroupAddon::new("git-commit-submit")
                            .align(InputGroupAddonAlignment::BlockEnd)
                            .justify_end()
                            .pb_2()
                            .when(ai_enabled, |addon| {
                                addon.gap_2().child(
                                    action_button("git-generate-commit-message")
                                        .debug_selector(|| "git-generate-commit-message".into())
                                        .secondary()
                                        .small()
                                        .flex_shrink_0()
                                        .label(t!("ai.generate_commit"))
                                        .loading(self.ai_commit_pending)
                                        .disabled(ai_reason.is_some() || self.ai_commit_pending)
                                        .when_some(ai_reason, |button, reason| {
                                            button.tooltip(reason)
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.generate_commit_message(window, cx)
                                        })),
                                )
                            })
                            .child(commit),
                    ),
            )
    }

    fn submit_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.current_worktree() || self.commit_disabled_reason(cx).is_some() {
            return;
        }
        let Some(Ok(expected)) = self.commit_state.clone() else {
            return;
        };
        let message = self.commit_message.read(cx).value().to_string();
        if let Some(amend) = &self.commit_amend {
            self.start_history_edit(
                HistoryEdit::Amend(amend.tip.clone(), expected, message),
                window,
                cx,
            );
            return;
        }
        let backend = self.backend.clone();
        self.cancel_ai_commit();
        self.staging = true;
        self.stage_error = None;
        self.commit_state_task = Task::ready(());
        let work = cx.background_spawn(async move { backend.create_commit(expected, message) });
        self.stage_task = cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            _ = this.update_in(cx, |panel, window, cx| {
                panel.staging = false;
                match result {
                    Ok(_) => panel.commit_message.update(cx, |input, cx| {
                        input.set_value("", window, cx);
                    }),
                    Err(error) => panel.report_operation_error(error, window, cx),
                }
                panel.refresh(window, cx);
                cx.notify();
            });
        });
        cx.notify();
    }

    pub(super) fn report_operation_error(
        &mut self,
        error: anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) {
        let message = error.to_string();
        super::super::toast::error(window, cx, message.clone());
        self.stage_error = Some(message);
    }

    pub(super) fn prepare_worktree_restore(
        &mut self,
        path: String,
        targets: Vec<(String, git::WorktreeFile, bool)>,
        directory: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writes_busy(cx)
            || !self.current_worktree()
            || !matches!(
                self.selection,
                Some(Selection::Worktree(Mode::All | Mode::Unstaged))
            )
            || !self.backend.supports_git_operations()
        {
            return;
        }
        let selection = self.selection.clone();
        let backend = self.backend.clone();
        let work = cx.background_spawn(async move {
            let paths = targets
                .iter()
                .map(|(path, _, _)| path.clone())
                .collect::<Vec<_>>();
            let versions = backend.file_versions(&paths);
            let mut edits = Vec::new();
            let mut hidden = 0;
            for ((path, status, is_hidden), version) in targets.into_iter().zip(versions) {
                let version = version
                    .filter(|version| version.status.as_ref() == Some(&status))
                    .ok_or_else(|| anyhow::anyhow!("{path}: {}", t!("git.worktree_changed")))?;
                if status.untracked || version.has_restorable_index_entry() {
                    hidden += usize::from(is_hidden);
                    edits.push(git::FileEdit {
                        path,
                        version,
                        action: FileAction::Restore,
                        block: None,
                    });
                }
            }
            anyhow::ensure!(!edits.is_empty(), t!("git.cannot_edit"));
            Ok::<_, anyhow::Error>((edits, directory.then_some(hidden)))
        });
        self.stage_task = cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            _ = this.update_in(cx, |this, window, cx| {
                if this.selection != selection || !this.current_worktree() || this.writes_busy(cx) {
                    return;
                }
                match result {
                    Ok((edits, hidden)) => {
                        this.confirm_restore_edits(path, edits, hidden, window, cx)
                    }
                    Err(error) => {
                        this.report_operation_error(error, window, cx);
                        cx.notify();
                    }
                }
            });
        });
    }

    fn confirm_restore_edits(
        &self,
        path: String,
        edits: Vec<git::FileEdit>,
        hidden: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let check = self.workspace.read_with(cx, |workspace, _| {
            edits.iter().try_for_each(|edit| {
                workspace.check_git_file(Path::new(&edit.path), FileAction::Restore)
            })
        });
        if let Ok(Err(error)) = check {
            super::super::toast::error(window, cx, error.to_string());
            return;
        }
        let untracked = edits
            .iter()
            .filter(|edit| {
                edit.version
                    .status
                    .as_ref()
                    .is_some_and(|status| status.untracked)
            })
            .map(|edit| edit.path.clone())
            .collect::<Vec<_>>();
        let discarding = !untracked.is_empty();
        let panel = cx.weak_entity();
        let selection = self.selection.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let panel = panel.clone();
            let selection = selection.clone();
            let title = if discarding {
                t!("git.discard_changes")
            } else if hidden.is_some() {
                t!("git.restore_directory_title")
            } else {
                t!("git.restore_file_title")
            };
            let count = edits.len();
            let detail = if discarding {
                t!("git.discard_untracked_detail")
            } else if hidden.is_some() {
                t!("git.restore_directory_detail", path = &path, count = count)
            } else {
                t!("git.restore_file_detail", path = &path)
            };
            let edits = edits.clone();
            dialog
                .centered()
                .width(px(440.))
                .title(title)
                .child(
                    div()
                        .debug_selector(|| format!("git-restore-count:{count}"))
                        .child(detail),
                )
                .when(discarding, |dialog| {
                    dialog.child(
                        v_flex()
                            .id("git-discard-untracked-files")
                            .max_h(px(224.))
                            .overflow_scroll()
                            .text_size(GIT_TEXT_SIZE)
                            .children(untracked.iter().map(|path| {
                                div()
                                    .debug_selector(|| format!("git-discard-untracked:{path}"))
                                    .whitespace_nowrap()
                                    .child(path.escape_debug().to_string())
                            })),
                    )
                })
                .when_some(hidden, |dialog, hidden| {
                    dialog.child(
                        div()
                            .debug_selector(|| format!("git-restore-hidden:{hidden}"))
                            .child(t!("git.restore_directory_hidden", count = hidden)),
                    )
                })
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            action_button("git-cancel-file-restore")
                                .debug_selector(|| "git-cancel-file-restore".into())
                                .secondary()
                                .label_size(GIT_TEXT_SIZE)
                                .label(t!("common.cancel"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            action_button("git-confirm-file-restore")
                                .debug_selector(|| "git-confirm-file-restore".into())
                                .danger()
                                .label_size(GIT_TEXT_SIZE)
                                .label(if discarding {
                                    t!("git.discard_changes")
                                } else {
                                    t!("git.restore")
                                })
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    _ = panel.update(cx, |this, cx| {
                                        if this.selection == selection {
                                            this.edit_worktree_files(edits.clone(), window, cx);
                                        }
                                    });
                                }),
                        ),
                )
        });
    }
}
