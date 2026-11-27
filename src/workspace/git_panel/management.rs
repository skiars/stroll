use gpui_component::{
    Icon, ThemeMode, WindowExt as _,
    button::{ButtonCustomVariant, DropdownButton},
    notification::Notification,
};

use super::*;
use crate::git::{
    ManageGit, ManagedBranch, ManagedTree, Management, WorktreeRemovalAction,
    WorktreeRemovalPreview,
};
use crate::workspace::{topbar_segment, topbar_segments};

mod push;
use push::PushDialog;

struct ActionColors {
    manage: Hsla,
    fetch: Hsla,
    pull: Hsla,
    upstream: Hsla,
    remotes: Hsla,
}

impl ActionColors {
    fn for_mode(mode: ThemeMode) -> Self {
        let [manage, fetch, pull, upstream, remotes] = match mode {
            ThemeMode::Light => [0x7544ad, 0x087587, 0x28734a, 0xa33d79, 0x2d619e],
            ThemeMode::Dark => [0xc4a4fa, 0x61c8de, 0x86d7a5, 0xef9ccc, 0x8cbdec],
        }
        .map(|color| rgb(color).into());
        Self {
            manage,
            fetch,
            pull,
            upstream,
            remotes,
        }
    }
}

fn action_icon(name: IconName, color: Hsla, enabled: bool) -> Icon {
    Icon::new(name)
        .with_size(px(14.))
        .when(enabled, |icon| icon.text_color(color))
}

fn remove_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    disabled: bool,
    colors: Palette,
    cx: &App,
) -> Button {
    icon_button(id, IconName::Trash, label)
        .custom(
            ButtonCustomVariant::new(cx)
                .foreground(colors.danger)
                .hover(colors.danger.alpha(0.12))
                .active(colors.danger.alpha(0.20)),
        )
        .when(!disabled, |button| button.cursor_pointer())
        .disabled(disabled)
}

fn management_error(error: &anyhow::Error, fallback: &'static str) -> String {
    let message = error.to_string();
    let upstream_failed = "git.push_upstream_failed";
    // Older helpers append command output after reporting that the push succeeded.
    if message.starts_with(t!(upstream_failed).as_ref())
        || message.starts_with(t!(upstream_failed, locale = "en").as_ref())
    {
        return t!(upstream_failed).into_owned();
    }

    // The SSH helper may return the same messages in its default language.
    [
        "git.management_busy",
        "git.management_changed",
        "git.invalid_branch_name",
        "git.branch_checked_out",
        "git.remote_required",
        "git.worktree_absolute_path",
        "git.worktree_contains_files",
        "git.worktree_open",
        "git.worktree_unreferenced_head",
        "git.pull_save_edits",
        "git.push_multiple_urls",
        "git.push_non_fast_forward",
        "git.push_preview_changed",
        "git.upgrade_management",
        "git.upgrade_push_review",
    ]
    .into_iter()
    .find(|key| message == t!(*key) || message == t!(*key, locale = "en"))
    .map_or_else(|| t!(fallback).into_owned(), |key| t!(key).into_owned())
}

enum OperationDialog {
    Management(WeakEntity<ManagementDialog>),
    Push(WeakEntity<PushDialog>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Trees,
    Branches,
    Remotes,
    Standalone,
}

#[derive(Clone)]
enum Form {
    Tree,
    Branch,
    Rename(git::Branch),
    Remote(Option<String>),
    Upstream(String),
    Confirm(SharedString, SharedString, ManageGit),
    RemoveTree {
        path: PathBuf,
        reference: String,
        preview: Option<Box<WorktreeRemovalPreview>>,
    },
}

impl Form {
    fn title(&self) -> Cow<'static, str> {
        match self {
            Self::Tree => t!("git.new_worktree"),
            Self::Branch => t!("git.new_branch"),
            Self::Rename(_) => t!("git.rename_branch"),
            Self::Remote(None) => t!("git.add_remote"),
            Self::Remote(Some(_)) => t!("git.edit_remote"),
            Self::Upstream(_) => t!("git.set_upstream"),
            Self::Confirm(title, ..) => Cow::Owned(title.to_string()),
            Self::RemoveTree { preview, .. } => {
                match preview.as_ref().map(|preview| &preview.action) {
                    Some(WorktreeRemovalAction::MoveDirectory { .. }) => {
                        t!("git.move_worktree_and_remove_record")
                    }
                    Some(WorktreeRemovalAction::RemoveRecord) => t!("git.remove_worktree_record"),
                    _ => t!("git.remove_worktree"),
                }
            }
        }
    }
}

struct ManagementDialog {
    panel: WeakEntity<GitPanel>,
    page: Page,
    data: Option<Management>,
    form: Option<Form>,
    fields: [Entity<InputState>; 3],
    loading: bool,
    pending: bool,
    error: Option<String>,
}

impl ManagementDialog {
    fn open(
        panel: &GitPanel,
        page: Page,
        form: Option<(Form, [String; 3])>,
        window: &mut Window,
        cx: &mut Context<GitPanel>,
    ) {
        let backend = panel.backend.clone();
        let needs_data = form
            .as_ref()
            .is_none_or(|(form, _)| !matches!(form, Form::Confirm(..) | Form::Upstream(_)));
        let fields = std::array::from_fn(|ix| {
            cx.new(|cx| {
                InputState::new(window, cx).default_value(
                    form.as_ref()
                        .map(|(_, values)| values[ix].as_str())
                        .unwrap_or_default(),
                )
            })
        });
        let owner = cx.weak_entity();
        let dialog = cx.new(|_| Self {
            panel: owner,
            page,
            form: form.map(|(form, _)| form),
            fields,
            data: None,
            loading: false,
            pending: false,
            error: None,
        });
        if needs_data {
            dialog.update(cx, |dialog, cx| dialog.load(backend, window, cx));
        }
        window.open_dialog(cx, move |surface, window, _| {
            surface
                .centered()
                .width((window.viewport_size().width - px(28.)).clamp(px(260.), px(550.)))
                .close_button(false)
                .child(dialog.clone())
                .on_ok(|_, _, _| false)
        });
    }

    fn load(&mut self, backend: git::Backend, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        let query = cx.background_spawn(async move { backend.management() });
        cx.spawn_in(window, async move |this, cx| {
            let result = query.await;
            _ = this.update_in(cx, |this, _, cx| {
                this.loading = false;
                match result {
                    Ok(data) => this.data = Some(data),
                    Err(error) => {
                        this.error = Some(management_error(&error, "git.management_unavailable"))
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_form(
        &mut self,
        form: Form,
        values: [String; 3],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        let has_fields = !matches!(form, Form::Confirm(..) | Form::RemoveTree { .. });
        self.form = Some(form);
        for (input, value) in self.fields.iter().zip(values) {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        if has_fields {
            self.fields[0].read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn make_operation(&self, cx: &App) -> Option<ManageGit> {
        let field = |index: usize| self.fields[index].read(cx).value().trim().to_owned();
        Some(match self.form.as_ref()? {
            Form::Confirm(_, _, operation) => operation.clone(),
            Form::RemoveTree { preview, .. } => ManageGit::RemoveReviewedTree(preview.clone()?),
            Form::Tree => {
                let branch = field(0);
                let existing = self
                    .data
                    .as_ref()?
                    .branches
                    .iter()
                    .any(|item| item.branch.label == branch);
                ManageGit::AddTree {
                    path: field(1),
                    branch,
                    start: (!existing).then(|| field(2)),
                }
            }
            Form::Branch => ManageGit::AddBranch {
                name: field(0),
                start: field(1),
            },
            Form::Rename(branch) => ManageGit::RenameBranch {
                name: branch.label.clone(),
                new_name: field(0),
                oid: branch.oid.clone(),
            },
            Form::Remote(None) => ManageGit::AddRemote {
                name: field(0),
                url: field(1),
            },
            Form::Remote(Some(name)) => ManageGit::SetRemoteUrl {
                name: name.clone(),
                url: field(0),
            },
            Form::Upstream(source) => {
                let upstream = field(0);
                let upstream = if upstream.starts_with("refs/remotes/") {
                    upstream
                } else {
                    format!("refs/remotes/{upstream}")
                };
                ManageGit::SetUpstream {
                    source: source.clone(),
                    upstream,
                }
            }
        })
    }

    fn run(&mut self, operation: ManageGit, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.weak_entity();
        let result = self.panel.update(cx, |panel, cx| {
            panel.run_management(
                operation,
                Some(OperationDialog::Management(dialog)),
                window,
                cx,
            )
        });
        match result {
            Ok(Ok(())) => {
                self.pending = true;
                self.error = None;
            }
            Ok(Err(error)) => self.error = Some(management_error(&error, "git.management_failed")),
            Err(_) => self.error = Some(t!("git.management_changed").into_owned()),
        }
        cx.notify();
    }

    fn start_confirmation(
        &mut self,
        title: impl Into<SharedString>,
        detail: impl Into<SharedString>,
        operation: ManageGit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_form(
            Form::Confirm(title.into(), detail.into(), operation),
            Default::default(),
            window,
            cx,
        );
    }

    fn preview_tree_removal(
        &mut self,
        backend: git::Backend,
        path: PathBuf,
        reference: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.form = Some(Form::RemoveTree {
            path: path.clone(),
            reference: reference.clone(),
            preview: None,
        });
        self.error = None;
        self.loading = true;
        let query =
            cx.background_spawn(async move { backend.preview_worktree_removal(&path, &reference) });
        cx.spawn_in(window, async move |this, cx| {
            let result = query.await;
            _ = this.update_in(cx, |this, _, cx| {
                this.loading = false;
                if let Some(Form::RemoveTree { preview, .. }) = &mut this.form {
                    match result {
                        Ok(result) => *preview = Some(Box::new(result)),
                        Err(error) => this.error = Some(error.to_string()),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn tree_row(
        &self,
        index: usize,
        item: &ManagedTree,
        colors: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let tree = &item.tree;
        let blocked = if item.current {
            Some(t!("git.current_checkout"))
        } else if tree.main {
            Some(t!("git.main_worktree"))
        } else if tree.locked {
            Some(t!("git.locked_worktree"))
        } else {
            None
        };
        let status = blocked.clone().unwrap_or_else(|| {
            if tree.missing {
                t!("git.worktree_directory_missing")
            } else if tree.prunable.is_some() {
                t!("git.worktree_registration_unavailable")
            } else if item.clean == Some(false) {
                t!("git.worktree_has_local_files")
            } else if item.clean == Some(true) {
                t!("git.clean_worktree")
            } else {
                t!("git.worktree_status_unknown")
            }
        });
        let path = crate::paths::display(&tree.root);
        let lock = ManageGit::LockTree {
            path: tree.root.clone(),
            locked: !tree.locked,
        };
        let target = tree.root.clone();
        let reference = tree.reference.clone();
        row(index, &tree.reference, path, Some(status.into()), colors)
            .when(!tree.main && !item.current && tree.available, |row| {
                row.child(
                    icon_button(
                        ("git-worktree-lock", index),
                        action_icon(
                            if tree.locked {
                                IconName::LockOpen
                            } else {
                                IconName::Lock
                            },
                            colors.warning,
                            !self.pending,
                        ),
                        if tree.locked {
                            t!("git.unlock_worktree")
                        } else {
                            t!("git.lock_worktree")
                        },
                    )
                    .disabled(self.pending)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run(lock.clone(), window, cx);
                    })),
                )
            })
            .child(
                remove_button(
                    ("git-worktree-remove", index),
                    t!("git.remove_worktree"),
                    self.loading || self.pending || blocked.is_some(),
                    colors,
                    cx,
                )
                .debug_selector(move || format!("git-worktree-remove:{index}"))
                .when_some(blocked, |button, reason| button.tooltip(reason))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Ok(backend) = this.panel.read_with(cx, |panel, _| panel.backend.clone())
                    {
                        this.preview_tree_removal(
                            backend,
                            target.clone(),
                            reference.clone(),
                            window,
                            cx,
                        );
                    }
                })),
            )
    }

    fn branch_row(
        &self,
        index: usize,
        item: &ManagedBranch,
        colors: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let branch = &item.branch;
        let rename = branch.clone();
        let delete = ManageGit::DeleteBranch {
            name: branch.label.clone(),
            oid: branch.oid.clone(),
        };
        let status = match &item.worktree {
            Some(path) => t!("git.branch_in_worktree", path = crate::paths::display(path)),
            None if item.merged => t!("git.branch_merged"),
            None => t!("git.branch_not_merged"),
        };
        row(index, &branch.label, status, None, colors).when(item.worktree.is_none(), |row| {
            row.child(
                icon_button(
                    ("git-branch-rename", index),
                    action_icon(IconName::Pencil, colors.accent, !self.pending),
                    t!("git.rename_branch"),
                )
                .disabled(self.pending)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.start_form(
                        Form::Rename(rename.clone()),
                        [rename.label.clone(), String::new(), String::new()],
                        window,
                        cx,
                    );
                })),
            )
            .child(
                remove_button(
                    ("git-branch-delete", index),
                    t!("git.delete_branch"),
                    self.pending || !item.merged,
                    colors,
                    cx,
                )
                .debug_selector(move || format!("git-branch-delete:{index}"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.run(delete.clone(), window, cx);
                })),
            )
        })
    }

    fn remote_row(
        &self,
        index: usize,
        remote: &git::GitRemote,
        colors: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let name = remote.name.clone();
        let url = remote.url.clone();
        let remove = ManageGit::RemoveRemote { name: name.clone() };
        let detail = t!("git.remove_remote_detail", remote = name).into_owned();
        row(index, &remote.name, remote.url.as_str(), None, colors)
            .child(
                icon_button(
                    ("git-remote-edit", index),
                    action_icon(IconName::Pencil, colors.accent, !self.pending),
                    t!("git.edit_remote"),
                )
                .disabled(self.pending)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.start_form(
                        Form::Remote(Some(name.clone())),
                        [url.clone(), String::new(), String::new()],
                        window,
                        cx,
                    );
                })),
            )
            .child(
                remove_button(
                    ("git-remote-remove", index),
                    t!("git.remove_remote"),
                    self.pending,
                    colors,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.start_confirmation(
                        t!("git.remove_remote"),
                        detail.clone(),
                        remove.clone(),
                        window,
                        cx,
                    );
                })),
            )
    }

    fn form_body(
        &self,
        form: &Form,
        colors: Palette,
        viewport_height: Pixels,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let field = |index: usize, label: Cow<'static, str>| {
            v_flex()
                .gap_1()
                .child(label.clone())
                .child(Input::new(&self.fields[index]).aria_label(label))
        };
        let mut body = v_flex().gap_2();
        body = match form {
            Form::Tree => body
                .child(field(0, t!("git.branch_name")))
                .child(field(1, t!("git.worktree_path")))
                .child(field(2, t!("git.worktree_start")))
                .child(message(t!("git.worktree_create_help"), colors.muted)),
            Form::Branch => body
                .child(field(0, t!("git.branch_name")))
                .child(field(1, t!("git.start_point"))),
            Form::Rename(_) => body.child(field(0, t!("git.branch_name"))),
            Form::Remote(name) => {
                if name.is_some() {
                    body.child(field(0, t!("git.remote_url")))
                } else {
                    body.child(field(0, t!("git.remote_name")))
                        .child(field(1, t!("git.remote_url")))
                }
            }
            Form::Upstream(_) => body
                .child(field(0, t!("git.upstream_branch")))
                .child(message(t!("git.upstream_help"), colors.muted)),
            Form::Confirm(_, detail, _) => body.child(message(detail.clone(), colors.ink)),
            Form::RemoveTree { preview, .. } => {
                body.when_some(preview.as_deref(), |body, preview| {
                    body.child(worktree_removal_details(preview, colors, viewport_height))
                })
            }
        };
        let destructive = matches!(
            form,
            Form::RemoveTree { .. } | Form::Confirm(_, _, ManageGit::RemoveRemote { .. })
        );
        let label = if self.pending {
            t!("git.management_running")
        } else {
            form.title()
        };
        body.child(
            h_flex()
                .justify_end()
                .gap_2()
                .child(
                    action_button("git-management-cancel")
                        .debug_selector(|| "git-management-cancel".into())
                        .secondary()
                        .label(t!("common.cancel"))
                        .disabled(
                            self.pending || self.loading && matches!(form, Form::RemoveTree { .. }),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.page == Page::Standalone {
                                window.close_dialog(cx);
                            } else {
                                if matches!(this.form, Some(Form::RemoveTree { .. }))
                                    && this.error.is_some()
                                    && let Ok(backend) =
                                        this.panel.read_with(cx, |panel, _| panel.backend.clone())
                                {
                                    this.load(backend, window, cx);
                                }
                                this.form = None;
                                this.error = None;
                                cx.notify();
                            }
                        })),
                )
                .child(
                    action_button("git-management-apply")
                        .debug_selector(|| "git-management-apply".into())
                        .when(destructive, |button| button.danger())
                        .when(!destructive, |button| button.primary())
                        .label(label)
                        .disabled(
                            self.pending
                                || self.loading
                                || matches!(form, Form::Tree) && self.data.is_none()
                                || matches!(form, Form::RemoveTree { preview: None, .. }),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let Some(operation) = this.make_operation(cx) {
                                this.run(operation, window, cx);
                            }
                        })),
                ),
        )
    }
}

fn worktree_removal_details(
    preview: &WorktreeRemovalPreview,
    colors: Palette,
    viewport_height: Pixels,
) -> impl IntoElement {
    let text = |value, color| message(value, color).px_1().py_1();
    let path = crate::paths::display(&preview.path);
    let detail = match &preview.action {
        WorktreeRemovalAction::DeleteDirectory => {
            t!("git.worktree_delete_directory_detail", path = path)
        }
        WorktreeRemovalAction::RemoveRecord => t!("git.worktree_remove_record_detail", path = path),
        WorktreeRemovalAction::MoveDirectory { destination, .. } => t!(
            "git.worktree_move_directory_detail",
            path = path,
            destination = crate::paths::display(destination)
        ),
    };
    let mut body = v_flex()
        .debug_selector(|| "git-worktree-removal-details".into())
        .id("git-worktree-removal-details")
        .max_h(viewport_height * 0.6)
        .overflow_y_scroll()
        .gap_1()
        .child(text(detail, colors.ink))
        .child(text(Cow::Owned(preview.reference.clone()), colors.muted));
    if let WorktreeRemovalAction::MoveDirectory { reason, .. } = &preview.action
        && !reason.is_empty()
    {
        body = body.child(text(
            t!("git.worktree_registration_reason", reason = reason),
            colors.ink,
        ));
    }
    if preview.detached_unreferenced {
        body = body.child(text(
            t!(
                "git.worktree_detached_removal_warning",
                head = preview.head.as_deref().unwrap_or("HEAD")
            ),
            colors.danger,
        ));
    }
    if preview.entries.is_empty() {
        let status = match preview.action {
            WorktreeRemovalAction::DeleteDirectory => Some(t!("git.clean_worktree")),
            WorktreeRemovalAction::MoveDirectory { .. } => Some(t!("git.worktree_empty_directory")),
            WorktreeRemovalAction::RemoveRecord => None,
        };
        if let Some(status) = status {
            body = body.child(text(status, colors.muted));
        }
    } else {
        let details = v_flex()
            .debug_selector(|| "git-worktree-removal-entries".into())
            .children(preview.entries.iter().map(|(status, path)| {
                let label = match status.as_str() {
                    "??" => t!("git.worktree_entry_untracked"),
                    "!!" => t!("git.worktree_entry_ignored"),
                    "" => Cow::Borrowed(""),
                    code => t!("git.worktree_entry_tracked", status = code.trim()),
                };
                let path = path.escape_debug().to_string();
                if label.is_empty() {
                    text(Cow::Owned(path), colors.ink)
                } else {
                    text(Cow::Owned(format!("{label}  {path}")), colors.ink)
                }
            }));
        body = body
            .child(details)
            .when(preview.entry_count > preview.entries.len(), |body| {
                body.child(text(
                    t!(
                        "git.worktree_more_entries",
                        count = preview.entry_count - preview.entries.len()
                    ),
                    colors.muted,
                ))
            });
    }
    body
}

fn row(
    index: usize,
    title: &str,
    description: impl Into<SharedString>,
    status: Option<SharedString>,
    colors: Palette,
) -> Stateful<Div> {
    h_flex()
        .id(("git-management-row", index))
        .gap_1()
        .py_1()
        .border_t_1()
        .border_color(colors.border)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().font_semibold().truncate().child(title.to_owned()))
                .child(
                    div()
                        .text_size(GIT_BADGE_SIZE)
                        .text_color(colors.muted)
                        .truncate()
                        .child(description.into()),
                )
                .when_some(status, |label, status| {
                    label.child(
                        div()
                            .text_size(GIT_BADGE_SIZE)
                            .text_color(colors.muted)
                            .child(status),
                    )
                }),
        )
}

impl Render for ManagementDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Palette::for_mode(cx.theme().mode);
        let can_create = !self.pending && !self.loading && self.data.is_some();
        let form = self.form.as_ref();
        let label = match self.page {
            Page::Trees => t!("git.new_worktree"),
            Page::Branches => t!("git.new_branch"),
            Page::Remotes => t!("git.add_remote"),
            Page::Standalone => t!("git.set_upstream"),
        };
        let heading = if let Some(form) = form {
            h_flex()
                .flex_1()
                .min_w_0()
                .font_semibold()
                .child(form.title())
                .into_any_element()
        } else if matches!(self.page, Page::Trees | Page::Branches) {
            topbar_segments(colors)
                .id("git-management-tabs")
                .role(Role::Group)
                .child(
                    topbar_segment("git-management-trees", self.page == Page::Trees, colors)
                        .label(t!("repository.worktrees"))
                        .icon(IconName::FolderGit2)
                        .icon_size(px(14.))
                        .disabled(self.pending)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page = Page::Trees;
                            cx.notify();
                        })),
                )
                .child(
                    topbar_segment(
                        "git-management-branches",
                        self.page == Page::Branches,
                        colors,
                    )
                    .label(t!("git.local_branches"))
                    .icon(IconName::GitBranch)
                    .icon_size(px(14.))
                    .disabled(self.pending)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.page = Page::Branches;
                        cx.notify();
                    })),
                )
                .into_any_element()
        } else {
            h_flex()
                .font_semibold()
                .child(t!("git.manage_remotes"))
                .into_any_element()
        };
        v_flex()
            .id("git-management-dialog")
            .gap_2()
            .w_full()
            .child(
                h_flex()
                    .gap_2()
                    .child(heading)
                    .child(div().flex_1().min_w_0())
                    .when(form.is_none() && self.page != Page::Standalone, |header| {
                        header.child(
                            action_button("git-management-new")
                                .small()
                                .secondary()
                                .icon(action_icon(IconName::Plus, colors.accent, can_create))
                                .label(label)
                                .disabled(!can_create)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let (form, values) = match this.page {
                                        Page::Trees => (
                                            Form::Tree,
                                            [String::new(), String::new(), "HEAD".to_owned()],
                                        ),
                                        Page::Branches => (
                                            Form::Branch,
                                            [String::new(), "HEAD".to_owned(), String::new()],
                                        ),
                                        _ => (Form::Remote(None), Default::default()),
                                    };
                                    this.start_form(form, values, window, cx);
                                })),
                        )
                    })
                    .child(
                        icon_button("git-management-close", IconName::X, t!("common.close"))
                            .debug_selector(|| "git-management-close".into())
                            .disabled(self.pending)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
            .when_some(self.error.as_ref(), |body, error| {
                body.child(message(error.clone(), colors.danger))
            })
            .when(self.loading, |body| {
                body.child(message(t!("git.management_loading"), colors.muted))
            })
            .when_some(form, |body, form| {
                body.child(self.form_body(form, colors, window.viewport_size().height, cx))
            })
            .when_some(
                self.data.as_ref().filter(|_| form.is_none()),
                |body, data| {
                    let list = v_flex()
                        .id("git-management-rows")
                        .max_h((window.viewport_size().height * 0.58).min(px(430.)))
                        .overflow_y_scroll();
                    let list = match self.page {
                        Page::Trees => list.children(
                            data.trees
                                .iter()
                                .enumerate()
                                .map(|(index, item)| self.tree_row(index, item, colors, cx)),
                        ),
                        Page::Branches => list.children(
                            data.branches
                                .iter()
                                .enumerate()
                                .map(|(index, item)| self.branch_row(index, item, colors, cx)),
                        ),
                        Page::Remotes => list
                            .children(
                                data.remotes
                                    .iter()
                                    .enumerate()
                                    .map(|(index, item)| self.remote_row(index, item, colors, cx)),
                            )
                            .when(data.remotes.is_empty(), |list| {
                                list.child(message(t!("git.no_remotes"), colors.muted))
                            }),
                        Page::Standalone => list,
                    };
                    body.child(list)
                },
            )
    }
}

struct UpstreamDialog {
    detail: SharedString,
    result: Option<Result<git::UpstreamComparison, String>>,
}

impl Render for UpstreamDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Palette::for_mode(cx.theme().mode);
        let mut content = v_flex()
            .id("git-upstream-comparison")
            .gap_2()
            .child(self.detail.clone());
        match self.result.as_ref() {
            Some(Ok(comparison)) => {
                for (title, commits) in [
                    (t!("git.outgoing_commits"), &comparison.outgoing),
                    (t!("git.incoming_commits"), &comparison.incoming),
                ] {
                    content = content.child(div().font_semibold().child(title)).children(
                        commits.iter().map(|commit| {
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_color(colors.muted)
                                        .child(commit.oid.clone()),
                                )
                                .child(div().min_w_0().truncate().child(commit.subject.clone()))
                        }),
                    );
                    if commits.is_empty() {
                        content = content.child(
                            div()
                                .text_color(colors.muted)
                                .child(t!("git.no_commits_in_direction")),
                        );
                    }
                }
            }
            Some(Err(error)) => content = content.child(message(error.clone(), colors.danger)),
            None => content = content.child(message(t!("git.management_loading"), colors.muted)),
        }
        content.max_h(px(350.)).overflow_y_scroll()
    }
}

impl GitPanel {
    pub(super) fn management_control(&self, cx: &Context<Self>) -> Button {
        let theme = cx.theme();
        let actions = ActionColors::for_mode(theme.mode);
        action_button("git-management")
            .secondary()
            .label_size(theme.control_font_size)
            .icon(action_icon(IconName::Settings2, actions.manage, true))
            .icon_size(px(14.))
            .label(t!("git.manage"))
            .tooltip(t!("git.manage_trees_branches"))
            .accessibility_label(t!("git.manage_trees_branches"))
            .debug_selector(|| "git-management".into())
            .on_click(cx.listener(|this, _, window, cx| {
                ManagementDialog::open(this, Page::Trees, None, window, cx);
            }))
    }

    pub(super) fn remote_control(
        &self,
        id: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let owner = cx.weak_entity();
        let mode = cx.theme().mode;
        let colors = Palette::for_mode(mode);
        let actions = ActionColors::for_mode(mode);
        let history = self.history.as_ref();
        let checkout = history
            .and_then(|history| history.checkout.as_ref())
            .cloned();
        let source =
            history.and_then(|history| Some((history.checkout.clone()?, history.head.clone()?)));
        let tracking = history
            .and_then(|history| {
                history
                    .branches
                    .iter()
                    .find(|branch| Some(&branch.reference) == checkout.as_ref())
            })
            .and_then(|branch| branch.tracking.clone())
            .filter(|tracking| !tracking.remote.is_empty() && tracking.remote != ".");
        let busy = self.writes_busy(cx);
        let can_push = !busy && source.is_some();
        let can_pull = can_push && tracking.is_some();
        let can_compare = tracking.as_ref().is_some_and(|tracking| !tracking.gone);
        let can_set_upstream = !busy && checkout.is_some();
        let push = quiet_button(format!("{id}-push"))
            .debug_selector(move || format!("{id}-push"))
            .with_size(px(24.))
            .px_1()
            .icon(action_icon(IconName::Upload, colors.accent, can_push))
            .icon_size(px(14.))
            .label(t!("git.push"))
            .disabled(!can_push)
            .on_click({
                let (owner, source) = (owner.clone(), source.clone());
                move |_, window, cx| {
                    if let Some((source, oid)) = source.clone() {
                        _ = owner.update(cx, |panel, cx| panel.open_push(source, oid, window, cx));
                    }
                }
            });
        div().debug_selector(move || id.into()).child(
            DropdownButton::new(id)
                .button(push)
                .menu_accessibility_label(t!("git.remote_operations"))
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu;
                    if let Some(checkout) = checkout.as_ref() {
                        menu = menu
                            .item(
                                PopupMenuItem::new(t!(
                                    "git.operation_checkout",
                                    branch = branch_name(checkout)
                                ))
                                .icon(action_icon(IconName::GitBranch, colors.muted, true))
                                .disabled(true),
                            )
                            .separator();
                    }
                    let fetch = PopupMenuItem::new(t!("git.fetch"))
                        .icon(action_icon(IconName::CloudDownload, actions.fetch, !busy))
                        .disabled(busy)
                        .on_click({
                            let owner = owner.clone();
                            let remote = tracking.as_ref().map(|tracking| tracking.remote.clone());
                            move |_, window, cx| {
                                _ = owner.update(cx, |panel, cx| {
                                    if let Err(error) = panel.run_management(
                                        ManageGit::Fetch {
                                            remote: remote.clone(),
                                        },
                                        None,
                                        window,
                                        cx,
                                    ) {
                                        panel.report_operation_error(error, window, cx);
                                    }
                                });
                            }
                        });
                    let pull = PopupMenuItem::new(t!("git.pull_ff"))
                        .icon(action_icon(IconName::Download, actions.pull, can_pull))
                        .disabled(!can_pull)
                        .on_click({
                            let (owner, source, tracking) =
                                (owner.clone(), source.clone(), tracking.clone());
                            move |_, window, cx| {
                                if let (Some((source, oid)), Some(tracking)) =
                                    (source.clone(), tracking.clone())
                                {
                                    _ = owner.update(cx, |panel, cx| {
                                        if let Err(error) = panel.run_management(
                                            ManageGit::Pull {
                                                source,
                                                oid,
                                                remote: tracking.remote,
                                                upstream: tracking.reference,
                                            },
                                            None,
                                            window,
                                            cx,
                                        ) {
                                            panel.report_operation_error(error, window, cx);
                                        }
                                    });
                                }
                            }
                        });
                    let compare = PopupMenuItem::new(t!("git.compare_upstream"))
                        .icon(action_icon(
                            IconName::GitCompareArrows,
                            colors.warning,
                            can_compare,
                        ))
                        .disabled(!can_compare)
                        .on_click({
                            let (owner, tracking, checkout) =
                                (owner.clone(), tracking.clone(), checkout.clone());
                            move |_, window, cx| {
                                if let (Some(tracking), Some(source)) =
                                    (tracking.clone(), checkout.clone())
                                {
                                    _ = owner.update(cx, |panel, cx| {
                                        panel.compare_tracking(source, tracking, window, cx)
                                    });
                                }
                            }
                        });
                    let upstream = PopupMenuItem::new(t!("git.set_upstream"))
                        .icon(action_icon(
                            IconName::Link2,
                            actions.upstream,
                            can_set_upstream,
                        ))
                        .disabled(!can_set_upstream)
                        .on_click({
                            let (owner, checkout, tracking) =
                                (owner.clone(), checkout.clone(), tracking.clone());
                            move |_, window, cx| {
                                if let Some(source) = checkout.clone() {
                                    let name = tracking
                                        .as_ref()
                                        .map(|item| branch_name(&item.reference).to_owned())
                                        .unwrap_or_default();
                                    _ = owner.update(cx, |panel, cx| {
                                        ManagementDialog::open(
                                            panel,
                                            Page::Standalone,
                                            Some((
                                                Form::Upstream(source),
                                                [name, String::new(), String::new()],
                                            )),
                                            window,
                                            cx,
                                        );
                                    });
                                }
                            }
                        });
                    menu.item(fetch)
                        .item(pull)
                        .item(compare)
                        .separator()
                        .item(upstream)
                        .item(
                            PopupMenuItem::new(t!("git.manage_remotes"))
                                .icon(action_icon(IconName::Server, actions.remotes, true))
                                .on_click({
                                    let owner = owner.clone();
                                    move |_, window, cx| {
                                        _ = owner.update(cx, |panel, cx| {
                                            ManagementDialog::open(
                                                panel,
                                                Page::Remotes,
                                                None,
                                                window,
                                                cx,
                                            )
                                        });
                                    }
                                }),
                        )
                }),
        )
    }

    pub(super) fn open_push(
        &self,
        source: String,
        oid: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        PushDialog::open(self, source, oid, window, cx);
    }

    fn compare_tracking(
        &self,
        source: String,
        tracking: git::Tracking,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let detail = t!(
            "git.compare_upstream_detail",
            branch = branch_name(&source),
            upstream = branch_name(&tracking.reference),
            ahead = tracking.ahead,
            behind = tracking.behind
        )
        .into_owned();
        let owner = cx.weak_entity();
        let comparison = cx.new(|_| UpstreamDialog {
            detail: detail.into(),
            result: None,
        });
        let backend = self.backend.clone();
        let reference = tracking.reference.clone();
        let task = cx.background_spawn(async move { backend.compare_upstream(source, reference) });
        let view = comparison.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let outcome = task
                .await
                .map_err(|error| management_error(&error, "git.compare_upstream_failed"));
            if let Some(view) = view.upgrade() {
                _ = view.update(cx, |view, cx| {
                    view.result = Some(outcome);
                    cx.notify();
                });
            }
        })
        .detach();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .centered()
                .title(t!("git.compare_upstream"))
                .width(px(420.))
                .child(comparison.clone())
                .footer(
                    h_flex().justify_end().child(
                        action_button("git-browse-upstream")
                            .secondary()
                            .label(t!("git.browse_upstream"))
                            .on_click({
                                let (owner, reference) =
                                    (owner.clone(), tracking.reference.clone());
                                move |_, window, cx| {
                                    window.close_dialog(cx);
                                    _ = owner.update(cx, |panel, cx| {
                                        panel.select_branch(reference.clone(), window, cx)
                                    });
                                }
                            }),
                    ),
                )
                .on_ok(|_, _, _| false)
        });
    }

    fn run_management(
        &mut self,
        operation: ManageGit,
        dialog: Option<OperationDialog>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(!self.writes_busy(cx), t!("git.management_busy"));
        let worktree_path = match &operation {
            ManageGit::RemoveTree { path, .. } => Some(path),
            ManageGit::RemoveReviewedTree(preview) => Some(&preview.path),
            _ => None,
        };
        if let Some(path) = worktree_path {
            let path = self
                .repository
                .ssh
                .as_ref()
                .map_or_else(|| path.clone(), |ssh| ssh.target().locator(path));
            let current = window.window_handle();
            let open = cx
                .windows()
                .into_iter()
                .filter(|handle| *handle != current)
                .any(|handle| {
                    handle
                        .update(cx, |root, _, cx| {
                            root.downcast::<gpui_component::Root>()
                                .ok()
                                .and_then(|root| {
                                    root.read(cx).view().clone().downcast::<Workspace>().ok()
                                })
                                .is_some_and(|workspace| {
                                    workspace
                                        .read(cx)
                                        .repository
                                        .as_ref()
                                        .is_some_and(|repo| repo.root == path)
                                })
                        })
                        .unwrap_or(false)
                });
            anyhow::ensure!(!open, t!("git.worktree_open"));
        }
        if matches!(operation, ManageGit::Pull { .. }) {
            let dirty = self.editor.has_pending_projection(self)
                || self
                    .workspace
                    .read_with(cx, |workspace, _| {
                        workspace
                            .files
                            .values()
                            .any(|file| file.is_dirty() || file.unconfirmed_save.is_some())
                    })
                    .unwrap_or(true);
            anyhow::ensure!(!dirty, t!("git.pull_save_edits"));
        }
        self.management_busy = true;
        let backend = self.backend.clone();
        let reviewed_removal = matches!(operation, ManageGit::RemoveReviewedTree(_));
        let (success, failure) = match operation {
            ManageGit::Fetch { .. } => (t!("git.fetch_finished"), "git.fetch_failed"),
            ManageGit::Pull { .. } => (t!("git.pull_finished"), "git.pull_failed"),
            ManageGit::Push { .. } | ManageGit::PushReviewed { .. } => {
                (t!("git.push_finished"), "git.push_failed")
            }
            ManageGit::RemoveTree { .. } | ManageGit::RemoveReviewedTree(_) => {
                (t!("git.worktree_removed"), "git.management_failed")
            }
            _ => (t!("git.management_finished"), "git.management_failed"),
        };
        let success = success.into_owned();
        let task = cx.background_spawn(async move { backend.manage(operation) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.management_busy = false;
                let handled = match dialog {
                    Some(OperationDialog::Management(dialog)) => {
                        dialog.upgrade().is_some_and(|dialog| {
                            dialog.update(cx, |dialog, cx| {
                                dialog.pending = false;
                                match &result {
                                    Ok(()) if dialog.page == Page::Standalone => {
                                        window.close_dialog(cx)
                                    }
                                    Ok(()) => {
                                        dialog.error = None;
                                        dialog.form = None;
                                        dialog.load(this.backend.clone(), window, cx);
                                    }
                                    Err(error) if reviewed_removal => {
                                        let changed = error.to_string();
                                        if (changed == t!("git.worktree_removal_changed")
                                            || changed
                                                == t!(
                                                    "git.worktree_removal_changed",
                                                    locale = "en"
                                                ))
                                            && let Some(Form::RemoveTree {
                                                path, reference, ..
                                            }) = dialog.form.clone()
                                        {
                                            dialog.preview_tree_removal(
                                                this.backend.clone(),
                                                path,
                                                reference,
                                                window,
                                                cx,
                                            );
                                            dialog.error = Some(
                                                t!("git.worktree_removal_changed").into_owned(),
                                            );
                                        } else {
                                            dialog.error = Some(changed);
                                        }
                                    }
                                    Err(error) => {
                                        dialog.error = Some(management_error(error, failure))
                                    }
                                }
                                cx.notify();
                            });
                            true
                        })
                    }
                    Some(OperationDialog::Push(dialog)) => dialog.upgrade().is_some_and(|dialog| {
                        dialog.update(cx, |dialog, cx| dialog.finished(&result, window, cx));
                        true
                    }),
                    None => false,
                };
                if !handled && let Err(error) = &result {
                    super::super::toast::error(window, cx, management_error(error, failure));
                }
                if result.is_ok() {
                    window.push_notification(
                        Notification::success(success).placement(super::super::toast::PLACEMENT),
                        cx,
                    );
                }
                this.refresh(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn management_messages_keep_user_guidance_and_hide_command_output() {
        let raw = anyhow::anyhow!(
            "Git read failed (exit status: 128)\nfatal: could not read credentials"
        );
        assert_eq!(
            management_error(&raw, "git.push_preview_unavailable"),
            t!("git.push_preview_unavailable")
        );
        let upstream = anyhow::anyhow!(
            "{} Git read failed (exit status: 128)",
            t!("git.push_upstream_failed", locale = "en")
        );
        assert_eq!(
            management_error(&upstream, "git.push_failed"),
            t!("git.push_upstream_failed")
        );
    }
}
