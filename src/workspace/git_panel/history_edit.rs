use gpui_component::{
    WindowExt as _,
    menu::{DropdownMenu as _, PopupMenu},
    radio::{Radio, RadioGroup},
};

use super::*;
use crate::git::{
    HistoryAction, HistoryEdit, HistoryPlan, HistoryProgress, HistoryResolution, HistoryTip,
    ResetMode, ResetPlan, RewriteKind, RewriteRecord, SavedChanges,
};

#[derive(Default)]
pub(super) struct HistoryPick {
    pub rows: Option<Rc<BTreeSet<SharedString>>>,
    anchor: Option<SharedString>,
    cursor: Option<SharedString>,
}

impl HistoryPick {
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Copy)]
pub(super) enum HistoryCommand {
    Reword,
    Squash,
    Rebase,
    Drop,
    Undo,
    Revert,
    Reset,
}

impl HistoryCommand {
    fn label(self) -> Cow<'static, str> {
        match self {
            Self::Reword => t!("git.history_edit_reword"),
            Self::Squash => t!("git.history_edit_squash"),
            Self::Rebase => t!("git.history_edit_from_here"),
            Self::Drop => t!("git.history_edit_drop"),
            Self::Undo => t!("git.history_edit_undo"),
            Self::Revert => t!("git.history_edit_revert"),
            Self::Reset => t!("git.reset_to_here"),
        }
    }
}

impl GitPanel {
    pub(super) fn pick_commit(
        &mut self,
        oid: &SharedString,
        modifiers: Modifiers,
        mouse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let additive = if cfg!(target_os = "macos") {
            modifiers.platform
        } else {
            modifiers.control
        };
        if mouse && !modifiers.shift && !additive {
            self.select_history_target(Some(oid), window, cx);
            return;
        }
        let previous = self
            .selection
            .as_ref()
            .and_then(Selection::target_commit)
            .map(SharedString::from);
        let pick = &mut self.history_pick;
        let mut rows = pick
            .rows
            .as_deref()
            .cloned()
            .unwrap_or_else(|| previous.clone().into_iter().collect());
        let anchor = pick
            .anchor
            .clone()
            .or(previous)
            .unwrap_or_else(|| oid.clone());
        if modifiers.shift {
            let index = |oid: &SharedString| self.commits.iter().position(|row| &row.oid == oid);
            if !additive {
                rows.clear();
            }
            if let (Some(start), Some(end)) = (index(&anchor), index(oid)) {
                rows.extend(
                    self.commits[start.min(end)..=start.max(end)]
                        .iter()
                        .map(|row| row.oid.clone()),
                );
            } else {
                rows.insert(oid.clone());
            }
            pick.anchor = Some(anchor);
        } else {
            if additive && mouse {
                if !rows.remove(oid) {
                    rows.insert(oid.clone());
                }
            } else {
                rows = [oid.clone()].into();
            }
            pick.anchor = Some(oid.clone());
        }
        pick.cursor = Some(oid.clone());
        let target = if !rows.contains(oid) {
            self.commits
                .iter()
                .find(|row| rows.contains(&row.oid))
                .map(|row| &row.oid)
                .unwrap_or(oid)
        } else {
            oid
        };
        let target = target.to_string();
        pick.rows = Some(Rc::new(rows));
        if let Some(index) = self.commits.iter().position(|row| &row.oid == oid) {
            self.history_scroll
                .scroll_to_reveal_item(index + usize::from(self.history.is_some()));
        }
        let selection = self
            .selection
            .as_ref()
            .filter(|selection| selection.target_commit() == Some(&target))
            .cloned()
            .unwrap_or(Selection::Commit(target, None));
        self.load_review(selection, false, window, cx);
        cx.notify();
    }

    pub(super) fn history_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.history_focus.is_focused(window) {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        match event.keystroke.key.as_str() {
            "up" | "down" if !modifiers.alt && !modifiers.control && !modifiers.platform => {
                let cursor = self
                    .history_pick
                    .cursor
                    .as_deref()
                    .or_else(|| self.selection.as_ref().and_then(Selection::target_commit));
                let index = self
                    .commits
                    .iter()
                    .position(|row| Some(row.oid.as_str()) == cursor);
                let next = match (index, event.keystroke.key.as_str()) {
                    (Some(index), "up") => index.saturating_sub(1),
                    (Some(index), _) => index + 1,
                    (None, _) => 0,
                };
                if let Some(oid) = self.commits.get(next).map(|row| row.oid.clone()) {
                    self.pick_commit(&oid, modifiers, false, window, cx);
                }
                cx.stop_propagation();
            }
            "f2" if modifiers == Modifiers::default()
                && cfg!(any(target_os = "linux", target_os = "windows"))
                && self.editable_tip().is_some() =>
            {
                let oid = match self.history_pick.rows.as_deref() {
                    Some(rows) if rows.len() == 1 => rows.first().map(SharedString::as_str),
                    Some(_) => None,
                    None => self.selection.as_ref().and_then(Selection::target_commit),
                };
                if let Some(row) = self
                    .commits
                    .iter()
                    .find(|row| oid == Some(row.oid.as_str()) && !row.merge)
                {
                    self.open_history_command(
                        HistoryCommand::Reword,
                        vec![row.oid.to_string()],
                        row.subject.clone(),
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }
            }
            _ => {}
        }
    }

    pub(super) fn editable_tip(&self) -> Option<HistoryTip> {
        let history = self.history.as_ref()?;
        let branch = history.checkout.as_ref()?;
        if !self.backend.supports_history_edit()
            || self.history_progress.is_some()
            || self.branch != *branch
            || !branch.starts_with("refs/heads/")
        {
            return None;
        }
        Some(HistoryTip {
            branch: branch.clone(),
            oid: history.head.clone()?,
        })
    }

    pub(super) fn history_commands(
        &self,
        oid: &SharedString,
        subject: &SharedString,
        merge: bool,
        owner: &WeakEntity<GitPanel>,
        cx: &App,
    ) -> Vec<PopupMenuItem> {
        let Some(tip) = self.editable_tip() else {
            return Vec::new();
        };
        let picked = self
            .history_pick
            .rows
            .as_ref()
            .filter(|rows| rows.contains(oid));
        let mut targets = vec![oid.to_string()];
        let actions = if let Some(rows) = picked.filter(|rows| rows.len() > 1) {
            targets = rows.iter().map(ToString::to_string).collect();
            vec![HistoryCommand::Squash, HistoryCommand::Drop]
        } else {
            let mut actions = Vec::new();
            if !merge {
                actions.extend([HistoryCommand::Reword, HistoryCommand::Rebase]);
                if tip.oid == oid.as_str() {
                    actions.push(HistoryCommand::Undo);
                }
                actions.extend([HistoryCommand::Drop, HistoryCommand::Revert]);
            }
            if tip.oid != oid.as_str() {
                actions.push(HistoryCommand::Reset);
            }
            actions
        };
        let busy = self.writes_busy(cx) || self.history_preparing;
        let mut items: Vec<_> = actions
            .into_iter()
            .map(|action| {
                let owner = owner.clone();
                let targets = targets.clone();
                let subject = subject.clone();
                PopupMenuItem::new(format!("{}…", action.label().trim_end_matches('…')))
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        _ = owner.update(cx, |panel, cx| {
                            panel.open_history_command(
                                action,
                                targets.clone(),
                                subject.clone(),
                                window,
                                cx,
                            )
                        });
                    })
            })
            .collect();
        if oid.as_str() == tip.oid
            && picked.is_none_or(|rows| rows.len() <= 1)
            && let Some(record) = &self.history_rewrite
        {
            let owner = owner.clone();
            let record = record.clone();
            items.push(
                PopupMenuItem::new(t!("git.history_rewrite_undo"))
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        _ = owner.update(cx, |panel, cx| {
                            panel.start_history_edit(
                                HistoryEdit::UndoRewrite(record.clone()),
                                window,
                                cx,
                            );
                        });
                    }),
            );
        }
        items
    }

    fn open_history_command(
        &mut self,
        command: HistoryCommand,
        targets: Vec<String>,
        subject: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tip) = self.editable_tip() else {
            return;
        };
        if self.writes_busy(cx) || self.history_preparing {
            return;
        }
        self.history_focus.focus(window, cx);
        if matches!(command, HistoryCommand::Revert) {
            let oid = targets[0].clone();
            let detail = t!(
                "git.history_edit_revert_detail",
                oid = git::short(&oid),
                subject = subject
            );
            HistoryDialog::open(
                command.label(),
                DialogBody::Confirm(detail.into(), HistoryEdit::Revert(tip, oid)),
                cx.weak_entity(),
                window,
                cx,
            );
            return;
        }
        let backend = self.backend.clone();
        let expected = tip.clone();
        let selected = targets.iter().cloned().collect::<HashSet<_>>();
        self.history_preparing = true;
        let task = cx.background_spawn(async move {
            if matches!(command, HistoryCommand::Reset) {
                backend
                    .reset_plan(tip, &targets[0])
                    .map(|plan| DialogBody::Reset(plan, subject))
            } else {
                backend
                    .history_plan(tip, &targets)
                    .and_then(|plan| prepare_dialog(command, plan, &selected, subject))
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |panel, window, cx| {
                panel.history_preparing = false;
                cx.notify();
                if panel.editable_tip().as_ref() != Some(&expected)
                    || panel.writes_busy(cx)
                    || !panel
                        .workspace
                        .read_with(cx, |workspace, _| {
                            workspace.view == super::super::WorkspaceView::Git
                        })
                        .unwrap_or(false)
                {
                    return;
                }
                let body = match result {
                    Ok(body) => body,
                    Err(error) => {
                        panel.report_operation_error(error, window, cx);
                        return;
                    }
                };
                HistoryDialog::open(command.label(), body, cx.weak_entity(), window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn notify_rewrite(&self, record: RewriteRecord, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::notification::Notification;
        let message = match record.kind {
            RewriteKind::Squash => t!("git.history_rewrite_squashed"),
            RewriteKind::Rebase => t!("git.history_rewrite_rebased"),
            RewriteKind::Amend => t!("git.history_rewrite_amended"),
        };
        let owner = cx.weak_entity();
        window.push_notification(
            Notification::success(message)
                .placement(super::super::toast::PLACEMENT)
                .action(move |_, _, cx| {
                    let owner = owner.clone();
                    let record = record.clone();
                    action_button("git-history-rewrite-undo")
                        .debug_selector(|| "git-history-rewrite-undo".into())
                        .small()
                        .secondary()
                        .label(t!("git.history_rewrite_undo_action"))
                        .on_click(cx.listener(move |notification, _, window, cx| {
                            notification.dismiss(window, cx);
                            _ = owner.update(cx, |panel, cx| {
                                panel.start_history_edit(
                                    HistoryEdit::UndoRewrite(record.clone()),
                                    window,
                                    cx,
                                );
                            });
                        }))
                })
                .autohide(true),
            cx,
        );
    }

    pub(super) fn start_history_edit(
        &mut self,
        edit: HistoryEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writes_busy(cx) {
            self.report_operation_error(anyhow::anyhow!(t!("git.git_busy")), window, cx);
            return;
        }
        let dirty = self
            .workspace
            .read_with(cx, |workspace, _| match &edit {
                HistoryEdit::Undo(_)
                | HistoryEdit::Amend(..)
                | HistoryEdit::Reset(_, ResetMode::Soft | ResetMode::Mixed)
                | HistoryEdit::Resume(_, HistoryResolution::KeepBackup) => false,
                HistoryEdit::Resume(_, HistoryResolution::Stage(path)) => {
                    workspace.tab_is_dirty(std::path::Path::new(path))
                }
                _ => workspace
                    .files
                    .values()
                    .any(|file| file.is_dirty() || file.unconfirmed_save.is_some()),
            })
            .unwrap_or(true);
        if dirty {
            self.report_operation_error(
                anyhow::anyhow!(t!("git.history_edit_save_first")),
                window,
                cx,
            );
            return;
        }
        let amend = matches!(edit, HistoryEdit::Amend(..));
        let review_worktree = match &edit {
            HistoryEdit::Undo(_) => Some(Mode::Staged),
            HistoryEdit::UndoRewrite(record) if record.kind == RewriteKind::Amend => {
                Some(Mode::Staged)
            }
            HistoryEdit::Reset(..) | HistoryEdit::UndoRewrite(_) | HistoryEdit::Amend(..) => {
                Some(Mode::All)
            }
            _ => None,
        };
        let update_commit = review_worktree.is_none()
            && !matches!(
                edit,
                HistoryEdit::Resume(
                    _,
                    HistoryResolution::Stage(_)
                        | HistoryResolution::Abort
                        | HistoryResolution::RestoreSaved
                        | HistoryResolution::KeepBackup
                )
            );
        let backend = self.backend.clone();
        self.cancel_ai_commit();
        self.staging = true;
        self.stage_error = None;
        self.commit_state_task = Task::ready(());
        let task = cx.background_spawn(async move {
            let result = backend.edit_history(edit);
            let progress = backend.history_progress();
            (result, progress)
        });
        self.stage_task = cx.spawn_in(window, async move |this, cx| {
            let (result, progress) = task.await;
            _ = this.update_in(cx, |panel, window, cx| {
                panel.staging = false;
                let completed = result.is_ok();
                if amend && completed {
                    panel.clear_commit_amend(window, cx);
                }
                if let Ok((_, Some(record))) = &result {
                    panel.notify_rewrite(record.clone(), window, cx);
                }
                if let Err(error) = &result
                    && progress
                        .as_ref()
                        .is_none_or(|progress| progress.conflicts.is_empty())
                {
                    panel.report_operation_error(anyhow::anyhow!(error.to_string()), window, cx);
                }
                let entered = progress.as_ref().is_some_and(|next| {
                    panel
                        .history_progress
                        .as_ref()
                        .is_none_or(|old| old.saved != next.saved)
                });
                panel.history_progress = progress;
                if let Some(mode) = if entered {
                    Some(Mode::All)
                } else if completed {
                    review_worktree
                } else {
                    None
                } {
                    panel.history_pick.clear();
                    panel.load_review(Selection::Worktree(mode), false, window, cx);
                } else if completed
                    && update_commit
                    && let Ok((Some(oid), _)) = result
                {
                    panel.history_pick.clear();
                    panel.load_review(Selection::Commit(oid, None), false, window, cx);
                    panel.history_scroll.scroll_to(ListOffset::default());
                }
                panel.refresh(window, cx);
                cx.notify();
            });
        });
        cx.notify();
    }

    pub(super) fn history_progress_banner(
        &self,
        progress: &HistoryProgress,
        colors: Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        if let Some(saved) = &progress.saved {
            return self
                .saved_changes_banner(progress, saved, colors, cx)
                .into_any_element();
        }
        let busy = self.writes_busy(cx);
        let blocked = !progress.conflicts.is_empty();
        let title = if progress.is_rebase() {
            t!("git.history_edit_rebase_paused")
        } else {
            t!("git.history_edit_revert_paused")
        };
        let location = progress.commit.as_deref().map(git::short);
        let detail = if blocked {
            t!(
                "git.history_edit_conflict_count",
                count = progress.conflicts.len()
            )
        } else {
            t!("git.history_edit_resume_detail")
        };
        let continue_from = progress.clone();
        let abort_from = progress.clone();
        v_flex()
            .debug_selector(|| "git-history-edit-progress".into())
            .flex_shrink_0()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.elevated)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .font_semibold()
                            .text_color(colors.warning)
                            .child(title),
                    )
                    .when_some(location, |row, oid| {
                        row.child(div().text_xs().child(oid.to_owned()))
                    }),
            )
            .child(div().text_xs().text_color(colors.muted).child(detail))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        action_button("git-history-continue")
                            .small()
                            .label(t!("git.history_edit_continue"))
                            .disabled(busy || blocked)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.start_history_edit(
                                    HistoryEdit::Resume(
                                        continue_from.clone(),
                                        HistoryResolution::Continue,
                                    ),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        action_button("git-history-abort")
                            .secondary()
                            .small()
                            .label(t!("git.history_edit_abort"))
                            .disabled(busy)
                            .on_click(cx.listener(move |_, _, window, cx| {
                                HistoryDialog::open(
                                    t!("git.history_edit_abort"),
                                    DialogBody::Confirm(
                                        t!("git.history_edit_abort_detail").into(),
                                        HistoryEdit::Resume(
                                            abort_from.clone(),
                                            HistoryResolution::Abort,
                                        ),
                                    ),
                                    cx.weak_entity(),
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
            .into_any_element()
    }

    fn saved_changes_banner(
        &self,
        progress: &HistoryProgress,
        saved: &SavedChanges,
        colors: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let reference = saved.reference.clone();
        let busy = self.writes_busy(cx);
        let restore = progress.clone();
        let keep = progress.clone();
        v_flex()
            .debug_selector(|| "git-history-saved-changes".into())
            .flex_shrink_0()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.elevated)
            .child(
                div()
                    .font_semibold()
                    .text_color(colors.warning)
                    .child(t!("git.history_backup_retained")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(colors.muted)
                    .child(if !saved.can_restore {
                        t!("git.history_backup_failed")
                    } else {
                        t!("git.history_backup_ready")
                    }),
            )
            .when(!progress.conflicts.is_empty(), |banner| {
                banner.child(div().text_xs().text_color(colors.muted).child(t!(
                    "git.history_edit_conflict_count",
                    count = progress.conflicts.len()
                )))
            })
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .when(saved.can_restore, |row| {
                        row.child(
                            action_button("git-history-restore-saved")
                                .small()
                                .label(t!("git.history_backup_restore"))
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.start_history_edit(
                                        HistoryEdit::Resume(
                                            restore.clone(),
                                            HistoryResolution::RestoreSaved,
                                        ),
                                        window,
                                        cx,
                                    );
                                })),
                        )
                    })
                    .child(
                        action_button("git-history-copy-backup")
                            .secondary()
                            .small()
                            .label(t!("git.history_backup_copy"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(reference.clone()));
                            }),
                    )
                    .child(
                        quiet_button("git-history-keep-backup")
                            .small()
                            .label(t!("git.history_backup_keep"))
                            .disabled(busy || !progress.conflicts.is_empty())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.start_history_edit(
                                    HistoryEdit::Resume(
                                        keep.clone(),
                                        HistoryResolution::KeepBackup,
                                    ),
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
    }

    pub(super) fn stage_resolution_item(
        &self,
        path: &str,
        owner: &WeakEntity<Self>,
        cx: &App,
    ) -> Option<PopupMenuItem> {
        let progress = self.history_progress.as_ref()?;
        if !self.current_worktree() || !progress.conflicts.iter().any(|item| item == path) {
            return None;
        }
        let progress = progress.clone();
        let owner = owner.clone();
        let path = path.to_owned();
        Some(
            PopupMenuItem::new(t!("git.history_edit_stage_resolution"))
                .disabled(self.writes_busy(cx))
                .on_click(move |_, window, cx| {
                    _ = owner.update(cx, |panel, cx| {
                        panel.start_history_edit(
                            HistoryEdit::Resume(
                                progress.clone(),
                                HistoryResolution::Stage(path.clone()),
                            ),
                            window,
                            cx,
                        )
                    });
                }),
        )
    }
}

fn prepare_dialog(
    command: HistoryCommand,
    mut plan: HistoryPlan,
    selected: &HashSet<String>,
    subject: SharedString,
) -> Result<DialogBody> {
    let original = || {
        plan.entries
            .iter()
            .map(|row| row.oid.clone())
            .collect::<Vec<_>>()
    };
    match command {
        HistoryCommand::Reword => {
            let message = plan.entries[0].message.clone();
            plan.entries[0].action = HistoryAction::Reword;
            Ok(DialogBody::Message(plan, message))
        }
        HistoryCommand::Squash => {
            plan.rewrite = Some(RewriteKind::Squash);
            let mut remaining = std::mem::take(&mut plan.entries);
            let mut group = remaining
                .extract_if(.., |row| selected.contains(&row.oid))
                .collect::<Vec<_>>();
            let message = group
                .iter()
                .map(|row| row.message.as_str())
                .filter(|message| !message.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            // Keep the group at its oldest commit and preserve the order of the other commits.
            group[0].action = HistoryAction::Reword;
            for row in group.iter_mut().skip(1) {
                row.action = HistoryAction::Fixup;
            }
            group.extend(remaining);
            plan.entries = group;
            Ok(DialogBody::Message(plan, message))
        }
        HistoryCommand::Undo => {
            anyhow::ensure!(plan.base.is_some(), t!("git.history_edit_no_parent"));
            Ok(DialogBody::Confirm(
                t!(
                    "git.history_edit_undo_detail",
                    oid = git::short(&plan.tip.oid),
                    subject = subject
                )
                .into(),
                HistoryEdit::Undo(plan.tip),
            ))
        }
        HistoryCommand::Drop => {
            let original = original();
            for row in &mut plan.entries {
                if selected.contains(&row.oid) {
                    row.action = HistoryAction::Drop;
                }
            }
            Ok(DialogBody::Plan(plan, original))
        }
        HistoryCommand::Rebase => {
            let original = original();
            plan.rewrite = Some(RewriteKind::Rebase);
            Ok(DialogBody::Plan(plan, original))
        }
        HistoryCommand::Revert | HistoryCommand::Reset => unreachable!(),
    }
}

enum DialogBody {
    Confirm(SharedString, HistoryEdit),
    Message(HistoryPlan, String),
    Plan(HistoryPlan, Vec<String>),
    Reset(ResetPlan, SharedString),
}

enum DialogContent {
    Confirm(SharedString, Option<HistoryEdit>),
    Reset(ResetPlan, SharedString, ResetMode),
    Message(HistoryPlan, Entity<TextareaState>, String),
    Plan {
        plan: HistoryPlan,
        original: Vec<String>,
        selected: String,
        messages: HashMap<String, Entity<TextareaState>>,
    },
}

struct HistoryDialog {
    owner: WeakEntity<GitPanel>,
    content: DialogContent,
    order_focus: FocusHandle,
    order_scroll: ScrollHandle,
}

impl HistoryDialog {
    fn open(
        title: Cow<'static, str>,
        body: DialogBody,
        owner: WeakEntity<GitPanel>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut focus = None;
        let mut wide = false;
        let state = cx.new(|cx| {
            let order_focus = cx.focus_handle();
            let content = match body {
                DialogBody::Confirm(label, edit) => DialogContent::Confirm(label, Some(edit)),
                DialogBody::Reset(plan, subject) => {
                    DialogContent::Reset(plan, subject, ResetMode::default())
                }

                DialogBody::Message(plan, message) => {
                    let input = Self::message_input(&message, window, cx);
                    focus = Some(input.focus_handle(cx));
                    DialogContent::Message(plan, input, message)
                }
                DialogBody::Plan(plan, original) => {
                    wide = true;
                    focus = Some(order_focus.clone());
                    DialogContent::Plan {
                        selected: plan.entries[0].oid.clone(),
                        original,
                        plan,
                        messages: HashMap::new(),
                    }
                }
            };
            Self {
                owner,
                content,
                order_focus,
                order_scroll: ScrollHandle::new(),
            }
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .centered()
                .width(px(if wide { 680. } else { 500. }))
                .title(title.clone())
                .child(state.clone())
                .on_ok(|_, _, _| false)
        });
        if let Some(focus) = focus {
            window.on_next_frame(move |window, cx| focus.focus(window, cx));
        }
    }

    fn message_input(
        message: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .default_value(message)
        });
        cx.subscribe(&input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        input
    }

    fn valid(&self, cx: &App) -> bool {
        match &self.content {
            DialogContent::Confirm(_, edit) => edit.is_some(),
            DialogContent::Reset(..) => true,
            DialogContent::Message(plan, input, original) => {
                let value = input.read(cx).value();
                git::valid_message(&value)
                    && (value.as_str() != original
                        || plan
                            .entries
                            .iter()
                            .any(|row| row.action == HistoryAction::Fixup))
            }
            DialogContent::Plan {
                plan,
                original,
                messages,
                ..
            } => {
                plan.valid()
                    && plan.entries.iter().zip(original).any(|(row, original)| {
                        &row.oid != original || row.action != HistoryAction::Pick
                    })
                    && plan.entries.iter().all(|row| {
                        row.action != HistoryAction::Reword
                            || messages
                                .get(&row.oid)
                                .is_some_and(|input| git::valid_message(&input.read(cx).value()))
                    })
            }
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.valid(cx) {
            return;
        }
        let edit = match &mut self.content {
            DialogContent::Confirm(_, edit) => edit.take().unwrap(),
            DialogContent::Reset(plan, _, mode) => HistoryEdit::Reset(plan.clone(), *mode),

            DialogContent::Message(plan, input, _) => {
                plan.entries[0].message = input.read(cx).value().to_string();
                HistoryEdit::Rebase(plan.clone())
            }
            DialogContent::Plan { plan, messages, .. } => {
                for row in &mut plan.entries {
                    if row.action == HistoryAction::Reword
                        && let Some(input) = messages.get(&row.oid)
                    {
                        row.message = input.read(cx).value().to_string();
                    }
                }
                HistoryEdit::Rebase(plan.clone())
            }
        };
        window.close_dialog(cx);
        _ = self
            .owner
            .update(cx, |panel, cx| panel.start_history_edit(edit, window, cx));
    }

    fn set_action(
        &mut self,
        oid: &str,
        action: HistoryAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let DialogContent::Plan {
            plan,
            selected,
            messages,
            ..
        } = &mut self.content
        else {
            return;
        };
        if let Some(row) = plan.entries.iter_mut().find(|row| row.oid == oid) {
            row.action = action;
            *selected = oid.to_owned();
            if action == HistoryAction::Reword {
                let input = messages
                    .entry(oid.to_owned())
                    .or_insert_with(|| Self::message_input(&row.message, window, cx));
                let focus = input.focus_handle(cx);
                window.on_next_frame(move |window, cx| focus.focus(window, cx));
            }
            cx.notify();
        }
    }

    fn move_row(&mut self, oid: &str, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        let DialogContent::Plan { plan, selected, .. } = &mut self.content else {
            return;
        };
        if let Some(from) = plan.entries.iter().position(|row| row.oid == oid) {
            let row = plan.entries.remove(from);
            let to = to.min(plan.entries.len());
            plan.entries.insert(to, row);
            selected.clone_from(&plan.entries[to].oid);
            self.order_focus.focus(window, cx);
            self.order_scroll.scroll_to_item(to);
            cx.notify();
        }
    }

    fn order_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let DialogContent::Plan { plan, selected, .. } = &mut self.content else {
            return;
        };
        let modifiers = event.keystroke.modifiers;
        if !self.order_focus.is_focused(window)
            || modifiers.control
            || modifiers.platform
            || modifiers.shift
        {
            return;
        }
        let Some(from) = plan.entries.iter().position(|row| &row.oid == selected) else {
            return;
        };
        let to = match event.keystroke.key.as_str() {
            "up" => from.saturating_sub(1),
            "down" => (from + 1).min(plan.entries.len() - 1),
            _ => return,
        };
        if modifiers.alt {
            let oid = selected.clone();
            self.move_row(&oid, to, window, cx);
        } else {
            selected.clone_from(&plan.entries[to].oid);
            self.order_scroll.scroll_to_item(to);
            cx.notify();
        }
        cx.stop_propagation();
    }
}

fn action_label(action: HistoryAction) -> Cow<'static, str> {
    match action {
        HistoryAction::Pick => t!("git.history_edit_action_pick"),
        HistoryAction::Reword => t!("git.history_edit_action_reword"),
        HistoryAction::Squash => t!("git.history_edit_action_squash"),
        HistoryAction::Fixup => t!("git.history_edit_action_fixup"),
        HistoryAction::Drop => t!("git.history_edit_action_drop"),
    }
}

#[derive(Clone)]
struct HistoryDrag {
    plan: EntityId,
    oid: String,
    subject: SharedString,
}

impl Render for HistoryDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Palette::for_mode(cx.theme().mode);
        div()
            .max_w(px(360.))
            .px_2()
            .py_1()
            .rounded_md()
            .bg(colors.elevated)
            .text_color(colors.ink)
            .truncate()
            .child(self.subject.clone())
    }
}

const RESET_MODES: [ResetMode; 4] = [
    ResetMode::Soft,
    ResetMode::Mixed,
    ResetMode::Keep,
    ResetMode::Hard,
];

fn reset_label(mode: ResetMode) -> Cow<'static, str> {
    match mode {
        ResetMode::Soft => t!("git.reset_soft"),
        ResetMode::Mixed => t!("git.reset_mixed"),
        ResetMode::Keep => t!("git.reset_keep"),
        ResetMode::Hard => t!("git.reset_hard"),
    }
}

impl HistoryDialog {
    fn reset_controls(
        plan: &ResetPlan,
        subject: &SharedString,
        mode: ResetMode,
        colors: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected = RESET_MODES.iter().position(|choice| *choice == mode);
        let dialog = cx.weak_entity();
        let modes = RadioGroup::vertical("git-reset-mode")
            .selected_index(selected)
            .on_change(move |index, _, cx| {
                _ = dialog.update(cx, |dialog, cx| {
                    if let DialogContent::Reset(_, _, mode) = &mut dialog.content {
                        *mode = RESET_MODES[*index];
                        cx.notify();
                    }
                });
            })
            .children(RESET_MODES.into_iter().enumerate().map(|(index, mode)| {
                Radio::new(("git-reset-choice", index))
                    .debug_selector(move || match mode {
                        ResetMode::Soft => "git-reset-soft".into(),
                        ResetMode::Mixed => "git-reset-mixed".into(),
                        ResetMode::Keep => "git-reset-keep".into(),
                        ResetMode::Hard => "git-reset-hard".into(),
                    })
                    .label(SharedString::from(reset_label(mode)))
            }));
        v_flex()
            .gap_2()
            .debug_selector(|| "git-reset-dialog".into())
            .child(div().font_semibold().child(format!(
                "{} · {}",
                git::short(&plan.target),
                subject
            )))
            .child(
                div()
                    .text_sm()
                    .text_color(colors.muted)
                    .child(t!("git.reset_count", count = plan.removed)),
            )
            .child(modes)
            .when(mode == ResetMode::Hard, |dialog| {
                dialog.child(
                    div()
                        .text_sm()
                        .text_color(colors.warning)
                        .debug_selector(|| "git-reset-hard-warning".into())
                        .child(t!("git.reset_hard_warning")),
                )
            })
    }
}

impl Render for HistoryDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Palette::for_mode(cx.theme().mode);
        let valid = self.valid(cx);
        let reset = matches!(self.content, DialogContent::Reset(..));
        let hard = matches!(self.content, DialogContent::Reset(_, _, ResetMode::Hard));
        let body = match &self.content {
            DialogContent::Confirm(label, _) => div().child(label.clone()).into_any_element(),
            DialogContent::Reset(plan, subject, mode) => {
                Self::reset_controls(plan, subject, *mode, colors, cx).into_any_element()
            }

            DialogContent::Message(plan, input, _) => v_flex()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.muted)
                        .child(t!("git.history_edit_range", count = plan.entries.len())),
                )
                .child(
                    div()
                        .debug_selector(|| "git-history-message".into())
                        .child(super::operations::commit_message_editor(input, cx)),
                )
                .into_any_element(),
            DialogContent::Plan {
                plan,
                selected,
                messages,
                ..
            } => {
                let id = cx.entity_id();
                let owner = cx.weak_entity();
                let rows = plan
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let selected = *selected == row.oid;
                        let oid = row.oid.clone();
                        let action = row.action;
                        let drag = HistoryDrag {
                            plan: id,
                            oid: oid.clone(),
                            subject: row.subject.clone().into(),
                        };
                        let menu_owner = owner.clone();
                        let mut subject: SharedString = row.subject.clone().into();
                        if subject.is_empty() {
                            subject = t!("git.empty_subject").into();
                        }
                        h_flex()
                            .id(row.oid.clone())
                            .debug_selector({
                                let oid = oid.clone();
                                move || format!("git-history-plan-row:{oid}")
                            })
                            .min_h(px(34.))
                            .px_2()
                            .gap_2()
                            .aria_selected(selected)
                            .when(selected, |row| row.bg(colors.hover))
                            .on_click(cx.listener({
                                let oid = oid.clone();
                                move |this, _, window, cx| {
                                    if let DialogContent::Plan { selected, .. } = &mut this.content
                                    {
                                        selected.clone_from(&oid);
                                        this.order_focus.focus(window, cx);
                                        cx.notify();
                                    }
                                }
                            }))
                            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
                            .can_drop(move |drag, _, _| {
                                drag.downcast_ref::<HistoryDrag>()
                                    .is_some_and(|drag| drag.plan == id)
                            })
                            .drag_over::<HistoryDrag>(move |style, _, _, _| {
                                style.bg(colors.accent.opacity(0.18))
                            })
                            .on_drop(cx.listener(move |this, drag: &HistoryDrag, window, cx| {
                                this.move_row(&drag.oid, index, window, cx);
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted)
                                    .child(git::short(&row.oid).to_owned()),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(subject))
                            .child(
                                Button::new(("git-history-plan-action", index))
                                    .debug_selector(move || {
                                        format!("git-history-plan-action:{index}")
                                    })
                                    .small()
                                    .ghost()
                                    .label(action_label(action))
                                    .dropdown_menu(move |mut menu: PopupMenu, _, _| {
                                        for choice in [
                                            HistoryAction::Pick,
                                            HistoryAction::Reword,
                                            HistoryAction::Squash,
                                            HistoryAction::Fixup,
                                            HistoryAction::Drop,
                                        ] {
                                            let owner = menu_owner.clone();
                                            let oid = oid.clone();
                                            menu = menu.item(
                                                PopupMenuItem::new(action_label(choice))
                                                    .checked(choice == action)
                                                    .on_click(move |_, window, cx| {
                                                        _ = owner.update(cx, |dialog, cx| {
                                                            dialog.set_action(
                                                                &oid, choice, window, cx,
                                                            )
                                                        });
                                                    }),
                                            );
                                        }
                                        menu
                                    }),
                            )
                    })
                    .collect::<Vec<_>>();
                let input = plan
                    .entries
                    .iter()
                    .find(|row| row.oid == *selected && row.action == HistoryAction::Reword)
                    .and_then(|_| messages.get(selected));
                v_flex()
                    .gap_2()
                    .child(div().text_xs().text_color(colors.muted).child(t!(
                        "git.history_edit_order_help",
                        count = plan.entries.len()
                    )))
                    .child(
                        v_flex()
                            .id("git-history-plan")
                            .track_focus(&self.order_focus)
                            .tab_stop(true)
                            .track_scroll(&self.order_scroll)
                            .overflow_y_scroll()
                            .max_h((window.viewport_size().height * 0.4).min(px(360.)))
                            .border_1()
                            .border_color(colors.border)
                            .rounded_md()
                            .on_key_down(cx.listener(Self::order_key))
                            .children(rows),
                    )
                    .when_some(input, |body, input| {
                        body.child(
                            div()
                                .debug_selector(|| "git-history-plan-message".into())
                                .child(super::operations::commit_message_editor(input, cx)),
                        )
                    })
                    .when(!plan.valid(), |body| {
                        body.child(
                            div()
                                .text_xs()
                                .text_color(colors.warning)
                                .child(t!("git.history_edit_plan_invalid")),
                        )
                    })
                    .into_any_element()
            }
        };
        v_flex()
            .gap_3()
            .capture_action(cx.listener(
                |this, action: &gpui_component::input::Enter, window, cx| {
                    if action.secondary && !action.shift {
                        cx.stop_propagation();
                        this.submit(window, cx);
                    }
                },
            ))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let key = &event.keystroke;
                if key.key == "enter" && key.modifiers.control {
                    cx.stop_propagation();
                    this.submit(window, cx);
                }
            }))
            .child(body)
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        action_button("git-history-edit-cancel")
                            .secondary()
                            .small()
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        action_button("git-history-edit-apply")
                            .small()
                            .debug_selector(|| "git-history-edit-apply".into())
                            .label(if reset {
                                t!("git.reset_apply")
                            } else {
                                t!("git.history_edit_apply")
                            })
                            .when(hard, |button| button.danger())
                            .disabled(!valid)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
