use std::path::Path;

use gpui_component::{
    WindowExt as _,
    input::{InputGroup, InputGroupAddon, InputGroupAddonAlignment, Textarea},
};

use super::*;
use crate::git::FileAction;

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

    fn commit_unavailable(&self, cx: &App) -> Option<Cow<'static, str>> {
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
            Some(Ok(state)) if !state.ready => return Some(t!("git.nothing_to_commit")),
            Some(Ok(_)) => {}
        }
        None
    }

    fn commit_disabled_reason(&self, cx: &App) -> Option<Cow<'static, str>> {
        self.commit_unavailable(cx).or_else(|| {
            self.commit_message
                .read(cx)
                .text()
                .chars()
                .all(char::is_whitespace)
                .then(|| t!("git.commit_message_required"))
        })
    }

    fn commit_ai(&self, cx: &App) -> Option<crate::ai::Snapshot> {
        self.workspace
            .read_with(cx, |workspace, _| workspace.ai.snapshot(false))
            .ok()
            .flatten()
    }

    fn ai_disabled_reason(&self, cx: &App) -> Option<Cow<'static, str>> {
        self.commit_unavailable(cx).or_else(|| {
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
        let reason = self.commit_disabled_reason(cx);
        let ai_enabled = self.commit_ai(cx).is_some();
        let ai_reason = ai_enabled.then(|| self.ai_disabled_reason(cx)).flatten();
        div()
            .w_full()
            .debug_selector(|| "git-commit-message-entry".into())
            .child(
                InputGroup::new("git-commit-controls")
                    .small()
                    .w_full()
                    .readonly(self.staging || !self.backend.supports_git_operations())
                    .input(
                        Textarea::new(&self.commit_message)
                            .aria_label(t!("git.commit_message"))
                            .text_size(px(text_base_size(cx)))
                            .line_height(relative(MESSAGE_LINE_HEIGHT))
                            .min_h(px(32.)),
                    )
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
                            .child(
                                action_button("git-create-commit")
                                    .debug_selector(|| "git-create-commit".into())
                                    .secondary()
                                    .small()
                                    .flex_shrink_0()
                                    .label(t!("git.commit_action"))
                                    .disabled(reason.is_some())
                                    .when_some(reason, |button, reason| button.tooltip(reason))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_commit(window, cx)
                                    })),
                            ),
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

    pub(super) fn confirm_file_restore(
        &self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.current_worktree() || self.writes_busy(cx) {
            return;
        }
        let Some(review) = &self.review else { return };
        let Some(&index) = review.by_path.get(path) else {
            return;
        };
        let Some(version) = review.versions.get(&index) else {
            return;
        };
        let edit = git::FileEdit {
            path: path.to_owned(),
            version: version.clone(),
            action: FileAction::Restore,
            block: None,
        };
        let check = self.workspace.read_with(cx, |workspace, _| {
            workspace.check_git_file(Path::new(path), FileAction::Restore)
        });
        if let Ok(Err(error)) = check {
            super::super::toast::error(window, cx, error.to_string());
            return;
        }
        let panel = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let panel = panel.clone();
            let edit = edit.clone();
            dialog
                .centered()
                .width(px(440.))
                .title(t!("git.restore_file_question"))
                .child(div().child(edit.path.clone()))
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            action_button("git-cancel-file-restore")
                                .secondary()
                                .label_size(GIT_TEXT_SIZE)
                                .label(t!("common.cancel"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            action_button("git-confirm-file-restore")
                                .danger()
                                .label_size(GIT_TEXT_SIZE)
                                .label(t!("git.restore"))
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    _ = panel.update(cx, |this, cx| {
                                        this.edit_worktree_file(edit.clone(), window, cx)
                                    });
                                }),
                        ),
                )
        });
    }
}
