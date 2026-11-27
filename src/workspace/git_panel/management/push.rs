use gpui_base::{Align, Placement};
use gpui_component::{
    ThemeStyled as _,
    checkbox::Checkbox,
    menu::DropdownMenu as _,
    scroll::popup_scrollbar,
    tooltip::{Positioner, TooltipRequest},
};

use super::*;
use crate::git::{PushPreview, PushTarget};
use crate::workspace::git_panel::review::{diff_counts, status_badge};

const MIN_PREVIEW_HEIGHT: Pixels = px(120.);

pub(super) struct PushDialog {
    panel: WeakEntity<GitPanel>,
    backend: git::Backend,
    source: String,
    oid: String,
    remotes: Option<Vec<String>>,
    remote: String,
    branch: Entity<InputState>,
    branch_names: Vec<SharedString>,
    show_completions: bool,
    completion_index: Option<usize>,
    completion_scroll: UniformListScrollHandle,
    branch_query: Task<()>,
    preview: Option<(PushTarget, Arc<PushPreview>)>,
    force: bool,
    set_upstream: bool,
    loading: bool,
    pushing: bool,
    error: Option<String>,
    query: Task<()>,
    commits_scroll: UniformListScrollHandle,
    files_scroll: UniformListScrollHandle,
    _branch_events: Subscription,
}

impl PushDialog {
    pub(super) fn open(
        panel: &GitPanel,
        source: String,
        oid: String,
        window: &mut Window,
        cx: &mut Context<GitPanel>,
    ) {
        let owner = cx.weak_entity();
        let backend = panel.backend.clone();
        let branch = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("git.push_target_branch"))
                .default_value(branch_name(&source))
        });
        let dialog = cx.new(|cx: &mut Context<Self>| {
            let events =
                cx.subscribe_in(&branch, window, |this, _, event, window, cx| match event {
                    InputEvent::Change if !this.pushing && this.remotes.is_some() => {
                        this.show_completions = true;
                        this.completion_index = None;
                        this.completion_scroll = UniformListScrollHandle::new();
                        this.request_preview(true, window, cx);
                    }
                    InputEvent::Focus => {
                        this.show_completions = true;
                        cx.notify();
                    }
                    InputEvent::Blur => {
                        this.show_completions = false;
                        this.completion_index = None;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } if this.show_completions => {
                        if let Some(index) = this.completion_index
                            && let Some(branch) = this.completions(cx).get(index)
                        {
                            this.complete_branch(branch.clone(), window, cx);
                        } else {
                            this.show_completions = false;
                            if this.loading {
                                this.request_preview(false, window, cx);
                            }
                            cx.notify();
                        }
                    }
                    _ => {}
                });
            Self {
                panel: owner,
                backend,
                source,
                oid,
                branch,
                branch_names: Vec::new(),
                show_completions: false,
                completion_index: None,
                completion_scroll: UniformListScrollHandle::new(),
                branch_query: Task::ready(()),
                remotes: None,
                remote: String::new(),
                preview: None,
                force: false,
                set_upstream: false,
                loading: true,
                pushing: false,
                error: None,
                query: Task::ready(()),
                commits_scroll: UniformListScrollHandle::new(),
                files_scroll: UniformListScrollHandle::new(),
                _branch_events: events,
            }
        });
        dialog.update(cx, |this, cx| this.load_options(window, cx));
        window.open_dialog(cx, move |surface, _, _| {
            let cancel = dialog.clone();
            git_operation_dialog(surface)
                .overlay_closable(false)
                .child(dialog.clone())
                .on_cancel(move |_, _, cx| {
                    cancel.update(cx, |dialog, cx| {
                        if dialog.show_completions && !dialog.completions(cx).is_empty() {
                            dialog.show_completions = false;
                            cx.notify();
                            false
                        } else {
                            !dialog.pushing
                        }
                    })
                })
                .on_ok(|_, _, _| false)
        });
    }

    fn load_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        let (backend, source) = (self.backend.clone(), self.source.clone());
        let query = cx.background_spawn(async move { backend.push_options(source) });
        self.query = cx.spawn_in(window, async move |this, cx| {
            let result = query.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Ok(options) => {
                        this.error = None;
                        this.remote = options.remote.unwrap_or_default();
                        this.set_upstream = options.set_upstream;
                        this.branch
                            .update(cx, |input, cx| input.set_value(options.branch, window, cx));
                        this.remotes = Some(options.remotes);
                        this.load_branches(window, cx);
                        this.request_preview(false, window, cx);
                    }
                    Err(error) => {
                        this.error = Some(management_error(&error, "git.push_options_unavailable"))
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn load_branches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.branch_names.clear();
        self.completion_index = None;
        self.completion_scroll = UniformListScrollHandle::new();
        self.branch_query = Task::ready(());
        if self.remote.is_empty() {
            return;
        }
        let backend = self.backend.clone();
        let remote = self.remote.clone();
        let query = cx.background_spawn(async move { backend.push_branches(remote) });
        self.branch_query = cx.spawn_in(window, async move |this, cx| {
            if let Ok(branches) = query.await {
                _ = this.update_in(cx, |this, _, cx| {
                    this.branch_names = branches.into_iter().map(SharedString::from).collect();
                    cx.notify();
                });
            }
        });
    }

    fn completions(&self, cx: &App) -> Vec<SharedString> {
        let value = self.branch.read(cx).value();
        let needle = value.trim();
        let mut prefix = Vec::new();
        let mut others = Vec::new();
        for name in &self.branch_names {
            if name.starts_with(needle) {
                prefix.push(name.clone());
            } else if name.contains(needle) {
                others.push(name.clone());
            }
        }
        prefix.extend(others);
        prefix
    }

    fn complete_branch(
        &mut self,
        branch: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_completions = false;
        self.completion_index = None;
        if self.branch.read(cx).value() != branch {
            self.branch
                .update(cx, |input, cx| input.set_value(branch, window, cx));
            self.request_preview(false, window, cx);
        }
        cx.notify();
    }

    fn branch_field(
        &self,
        disabled: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let priority = window.anchored_popup_priority(&self.branch.read(cx).focus_handle(cx), cx);
        let candidates = if !disabled && self.show_completions {
            self.completions(cx)
        } else {
            Vec::new()
        };
        let count = candidates.len();
        let active = self.completion_index;
        let dialog = cx.weak_entity();
        let scroll = self.completion_scroll.clone();
        let height = px(29.);
        h_flex()
            .id("git-push-branch")
            .debug_selector(|| "git-push-branch".into())
            .relative()
            .w(px(220.))
            .min_w_0()
            .flex_shrink_1()
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let key = event.keystroke.key.as_str();
                if !matches!(key, "down" | "up") {
                    return;
                }
                let count = this.completions(cx).len();
                if count == 0 {
                    return;
                }
                let next = match (this.completion_index, key) {
                    (Some(index), "down") => (index + 1) % count,
                    (Some(index), _) => (index + count - 1) % count,
                    (None, "down") => 0,
                    (None, _) => count - 1,
                };
                this.show_completions = true;
                this.completion_index = Some(next);
                this.completion_scroll
                    .scroll_to_item(next, ScrollStrategy::Nearest);
                cx.stop_propagation();
                cx.notify();
            }))
            .child(
                Input::new(&self.branch)
                    .small()
                    .w_full()
                    .aria_label(t!("git.push_target_branch"))
                    .disabled(disabled),
            )
            .when(count != 0, |field| {
                let list = uniform_list("git-push-branches-list", count, move |range, _, _| {
                    range
                        .map(|index| {
                            let label = candidates[index].clone();
                            let selector = format!("git-push-branch-option:{label}");
                            let dialog = dialog.clone();
                            quiet_button(("git-push-branch-option", index))
                                .debug_selector(move || selector.clone())
                                .h(height)
                                .w_full()
                                .px_2()
                                .selected(active == Some(index))
                                .tab_stop(false)
                                .role(Role::ListBoxOption)
                                .accessibility_label(label.clone())
                                .child(div().w_full().text_left().truncate().child(label.clone()))
                                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                    _ = dialog.update(cx, |dialog, cx| {
                                        dialog.complete_branch(label.clone(), window, cx)
                                    });
                                    cx.stop_propagation();
                                })
                        })
                        .collect::<Vec<_>>()
                })
                .h(height * count.min(7) as f32)
                .w_full()
                .track_scroll(&scroll);
                field.child(
                    deferred(
                        div()
                            .id("git-push-branch-suggestions")
                            .absolute()
                            .top_full()
                            .left_0()
                            .w_full()
                            .mt_1()
                            .role(Role::ListBox)
                            .aria_label(t!("git.push_branch_suggestions"))
                            .occlude()
                            .popover_style(cx)
                            .p_1()
                            .on_mouse_down_out(cx.listener(
                                |this, event: &MouseDownEvent, _, cx| {
                                    if !this
                                        .branch
                                        .read(cx)
                                        .input_bounds()
                                        .contains(&event.position)
                                    {
                                        this.show_completions = false;
                                        this.completion_index = None;
                                        cx.notify();
                                    }
                                },
                            ))
                            .child(
                                div().relative().child(list).child(
                                    div().absolute().inset_0().child(popup_scrollbar(&scroll)),
                                ),
                            ),
                    )
                    .with_priority(priority),
                )
            })
    }

    fn target(&self, cx: &App) -> Option<PushTarget> {
        let branch = self.branch.read(cx).value();
        let branch = branch.trim();
        (!self.remote.is_empty() && !branch.is_empty()).then(|| PushTarget {
            source: self.source.clone(),
            oid: self.oid.clone(),
            remote: self.remote.clone(),
            branch: branch.to_owned(),
        })
    }

    fn request_preview(&mut self, debounce: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.query = Task::ready(());
        self.force = false;
        self.loading = false;
        cx.notify();
        let Some(target) = self.target(cx) else {
            return;
        };
        self.loading = true;
        let backend = self.backend.clone();
        self.query = cx.spawn_in(window, async move |this, cx| {
            if debounce {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
            }
            let requested = target.clone();
            let task = cx.background_spawn(async move { backend.preview_push(requested) });
            let result = task.await;
            _ = this.update_in(cx, |this, _, cx| {
                if this.target(cx).as_ref() != Some(&target) {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(preview) => {
                        this.preview = Some((target, Arc::new(preview)));
                        this.commits_scroll = UniformListScrollHandle::new();
                        this.files_scroll = UniformListScrollHandle::new();
                        this.error = None;
                    }
                    Err(error) => {
                        this.error = Some(management_error(&error, "git.push_preview_unavailable"))
                    }
                }
                cx.notify();
            });
        });
    }

    fn current_preview(&self, cx: &App) -> Option<&(PushTarget, Arc<PushPreview>)> {
        self.preview.as_ref().filter(|(target, _)| {
            !self.loading && self.error.is_none() && self.target(cx).as_ref() == Some(target)
        })
    }

    fn operation(&self, cx: &App) -> Option<ManageGit> {
        let (target, preview) = self.current_preview(cx)?;
        if self.pushing
            || preview.overwritten != 0 && !self.force
            || preview.lease.target_oid.as_ref() == Some(&target.oid) && !self.set_upstream
        {
            return None;
        }
        Some(ManageGit::PushReviewed {
            target: target.clone(),
            lease: preview.lease.clone(),
            force: self.force,
            set_upstream: self.set_upstream,
        })
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(operation) = self.operation(cx) else {
            return;
        };
        let dialog = OperationDialog::Push(cx.weak_entity());
        match self.panel.update(cx, |panel, cx| {
            panel.run_management(operation, Some(dialog), window, cx)
        }) {
            Ok(Ok(())) => {
                self.pushing = true;
                self.error = None;
            }
            Ok(Err(error)) => self.error = Some(management_error(&error, "git.push_failed")),
            Err(_) => self.error = Some(t!("git.management_changed").into_owned()),
        }
        cx.notify();
    }

    pub(super) fn finished(
        &mut self,
        result: &Result<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pushing = false;
        match result {
            Ok(()) => window.close_dialog(cx),
            Err(error) => {
                self.force = false;
                self.error = Some(management_error(error, "git.push_failed"));
            }
        }
        cx.notify();
    }

    fn preview_content(
        &self,
        preview: &Arc<PushPreview>,
        window: &Window,
        colors: Palette,
        cx: &App,
    ) -> impl IntoElement {
        let reserved_height = if self.error.is_some() {
            px(397.)
        } else {
            px(332.)
        };
        let max_height = (window.viewport_size().height - reserved_height).clamp(px(60.), px(320.));
        let commit_row_height = px(36.);
        let file_row_height = px(27.);
        let title = |label: Cow<'static, str>| div().px_2().py_1().font_semibold().child(label);
        let rows = |height, list: AnyElement, scroll| {
            div()
                .relative()
                .w_full()
                .h(height)
                .child(list)
                .child(div().absolute().inset_0().child(popup_scrollbar(scroll)))
        };
        let commits = Arc::clone(preview);
        let commit_count = commits.outgoing.len();
        let commit_height = (commit_row_height * commit_count as f32).min(max_height);
        let commit_list =
            uniform_list("git-push-commits-list", commit_count, move |range, _, _| {
                range
                    .map(|index| {
                        let commit = &commits.outgoing[index];
                        let subject = SharedString::from(commit.subject.clone());
                        v_flex()
                            .id(("git-push-commit", index))
                            .debug_selector(move || format!("git-push-commit:{index}"))
                            .h(commit_row_height)
                            .justify_center()
                            .px_2()
                            .managed_tooltip_request(true, {
                                let subject = subject.clone();
                                move |bounds, _, _| {
                                    let subject = subject.clone();
                                    Some(
                                        TooltipRequest::new(bounds, move |window, cx| {
                                            Tooltip::new(subject.clone()).m_0().build(window, cx)
                                        })
                                        .with_positioner(move || {
                                            Positioner::side(bounds)
                                                .placement(Placement::Bottom)
                                                .align(Align::Start)
                                                .offset(px(4.))
                                        })
                                        .paint_only(),
                                    )
                                }
                            })
                            .child(div().min_w_0().truncate().child(subject))
                            .child(
                                div()
                                    .text_size(GIT_BADGE_SIZE)
                                    .text_color(colors.muted)
                                    .child(commit.oid.clone()),
                            )
                    })
                    .collect::<Vec<_>>()
            })
            .w_full()
            .h(commit_height)
            .track_scroll(&self.commits_scroll);
        let files = Arc::clone(preview);
        let file_count = files.files.len();
        let file_height = (file_row_height * file_count as f32).min(max_height);
        let file_list = uniform_list("git-push-files-list", file_count, move |range, _, cx| {
            range
                .map(|index| {
                    let file = &files.files[index];
                    let path = SharedString::from(match &file.old_path {
                        Some(old) => format!("{old} → {}", file.path),
                        None => file.path.clone(),
                    });
                    h_flex()
                        .id(("git-push-file", index))
                        .debug_selector(move || format!("git-push-file:{index}"))
                        .h(file_row_height)
                        .gap_2()
                        .px_2()
                        .managed_tooltip({
                            let path = path.clone();
                            move |window, cx| Tooltip::new(path.clone()).build(window, cx)
                        })
                        .child(status_badge(file, colors, GIT_TEXT_SIZE.as_f32(), cx))
                        .child(div().flex_1().min_w_0().truncate().child(path))
                        .when_some(file.counts, |row, counts| {
                            row.child(diff_counts(counts, GIT_BADGE_SIZE, cx))
                        })
                })
                .collect::<Vec<_>>()
        })
        .w_full()
        .h(file_height)
        .track_scroll(&self.files_scroll);
        let section = |growth| {
            v_flex()
                .flex_1()
                .flex_grow(growth)
                .min_w_0()
                .self_stretch()
                .rounded_md()
                .border_1()
                .border_color(colors.border)
                .overflow_hidden()
        };
        let counts = preview
            .files
            .iter()
            .filter_map(|file| file.counts)
            .fold((0, 0), |(adds, dels), (add, del)| (adds + add, dels + del));

        v_flex()
            .id("git-push-preview")
            .debug_selector(|| "git-push-preview".into())
            .gap_2()
            .when(
                preview.lease.target_oid.as_deref() == Some(self.oid.as_str()),
                |view| view.child(message(t!("git.push_up_to_date"), colors.muted)),
            )
            .when(preview.overwritten != 0, |view| {
                view.child(
                    div()
                        .id("git-push-overwrite-warning")
                        .debug_selector(|| "git-push-overwrite-warning".into())
                        .child(message(
                            t!("git.push_overwrite", count = preview.overwritten),
                            colors.danger,
                        )),
                )
            })
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .min_h(MIN_PREVIEW_HEIGHT)
                    .child(
                        section(37.)
                            .id("git-push-commits-panel")
                            .debug_selector(|| "git-push-commits-panel".into())
                            .child(title(t!("git.push_commits_summary", count = commit_count)))
                            .when(commit_count == 0, |section| {
                                section.child(div().px_2().pb_2().child(message(
                                    t!("git.no_commits_in_direction"),
                                    colors.muted,
                                )))
                            })
                            .when(commit_count > 0, |section| {
                                section.child(rows(
                                    commit_height,
                                    commit_list.into_any_element(),
                                    &self.commits_scroll,
                                ))
                            }),
                    )
                    .child(
                        section(63.)
                            .id("git-push-files-panel")
                            .debug_selector(|| "git-push-files-panel".into())
                            .child(
                                h_flex()
                                    .justify_between()
                                    .flex_wrap()
                                    .child(title(t!("git.push_files_summary", count = file_count)))
                                    .when(file_count > 0, |heading| {
                                        heading.child(div().px_2().child(diff_counts(
                                            counts,
                                            GIT_BADGE_SIZE,
                                            cx,
                                        )))
                                    }),
                            )
                            .when(file_count == 0, |section| {
                                section.child(
                                    div().px_2().pb_2().child(message(
                                        t!("git.push_no_file_changes"),
                                        colors.muted,
                                    )),
                                )
                            })
                            .when(file_count > 0, |section| {
                                section.child(rows(
                                    file_height,
                                    file_list.into_any_element(),
                                    &self.files_scroll,
                                ))
                            }),
                    ),
            )
    }
}

impl Render for PushDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Palette::for_mode(cx.theme().mode);
        let remotes = self.remotes.clone().unwrap_or_default();
        let no_remotes = remotes.is_empty();
        let remote = self.remote.clone();
        let owner = cx.weak_entity();
        let current = self.current_preview(cx);
        let can_force = current.is_some_and(|(_, preview)| preview.overwritten != 0);
        let new_branch = current
            .filter(|(_, preview)| preview.lease.target_oid.is_none())
            .map(|(_, preview)| preview.new_branch_base.as_ref());
        let stale = self.preview.is_some() && current.is_none();
        let force = self.force;
        let remote_label: SharedString = if remote.is_empty() {
            t!("git.push_choose_remote").into()
        } else {
            remote.clone().into()
        };
        let target = self.target(cx);
        v_flex()
            .id("git-push-dialog")
            .debug_selector(|| "git-push-dialog".into())
            .gap_3()
            .w_full()
            .child(
                h_flex()
                    .gap_2()
                    .pb_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(icon(IconName::Upload, colors.accent, 16.))
                    .child(div().font_semibold().child(t!("git.push")))
                    .child(div().flex_1().min_w_0())
                    .child(
                        h_flex()
                            .gap_1()
                            .min_w_0()
                            .text_color(colors.muted)
                            .child(icon(IconName::GitBranch, colors.muted, 14.))
                            .child(div().truncate().child(branch_name(&self.source).to_owned()))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(GIT_BADGE_SIZE)
                                    .child(git::short(&self.oid).to_owned()),
                            ),
                    )
                    .child(
                        icon_button("git-push-close", IconName::X, t!("common.close"))
                            .disabled(self.pushing)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(colors.muted)
                            .child(t!("git.push_to")),
                    )
                    .child(
                        action_button("git-push-remote")
                            .debug_selector(|| "git-push-remote".into())
                            .small()
                            .secondary()
                            .min_w(px(95.))
                            .max_w(px(116.))
                            .accessibility_label(format!(
                                "{} · {remote_label}",
                                t!("git.push_remote")
                            ))
                            .label(remote_label)
                            .disabled(no_remotes || self.pushing)
                            .dropdown_caret(true)
                            .dropdown_menu(move |menu, _, _| {
                                remotes.iter().fold(menu, |menu, name| {
                                    let chosen = name.clone();
                                    let owner = owner.clone();
                                    menu.item(
                                        PopupMenuItem::new(chosen.clone())
                                            .checked(chosen == remote)
                                            .on_click(move |_, window, cx| {
                                                _ = owner.update(cx, |dialog, cx| {
                                                    if chosen != dialog.remote {
                                                        dialog.remote.clone_from(&chosen);
                                                        dialog.load_branches(window, cx);
                                                        dialog.request_preview(false, window, cx);
                                                    }
                                                });
                                            }),
                                    )
                                })
                            }),
                    )
                    .child(
                        div()
                            .id("git-push-target-arrow")
                            .debug_selector(|| "git-push-target-arrow".into())
                            .flex_shrink_0()
                            .child(icon(IconName::ArrowRight, colors.muted, 14.)),
                    )
                    .child(self.branch_field(no_remotes || self.pushing, window, cx))
                    .child(
                        div()
                            .id("git-push-preview-status")
                            .debug_selector(|| "git-push-preview-status".into())
                            .role(Role::Status)
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(GIT_BADGE_SIZE)
                            .text_color(colors.muted)
                            .when(self.remotes.as_ref().is_some_and(Vec::is_empty), |status| {
                                status.child(t!("git.no_remotes"))
                            })
                            .when_some(new_branch, |status, baseline| {
                                let detail: SharedString = baseline
                                    .map_or_else(
                                        || t!("git.push_new_target"),
                                        |base| t!("git.push_new_target_base", base = base),
                                    )
                                    .into();
                                status
                                    .child(
                                        div()
                                            .id("git-push-new-target")
                                            .debug_selector(|| "git-push-new-target".into())
                                            .truncate()
                                            .child(t!("git.push_new_target")),
                                    )
                                    .managed_tooltip(move |window, cx| {
                                        Tooltip::new(detail.clone()).build(window, cx)
                                    })
                            }),
                    ),
            )
            .when_some(self.error.as_ref(), |body, error| {
                body.child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .id("git-push-error")
                                .debug_selector(|| "git-push-error".into())
                                .role(Role::Alert)
                                .flex_1()
                                .min_w_0()
                                .max_h(px(64.))
                                .overflow_y_scroll()
                                .text_size(GIT_TEXT_SIZE)
                                .text_color(colors.danger)
                                .child(error.clone()),
                        )
                        .when(target.is_some() || self.remotes.is_none(), |row| {
                            row.child(
                                action_button("git-push-refresh-preview")
                                    .debug_selector(|| "git-push-refresh-preview".into())
                                    .secondary()
                                    .icon(IconName::RefreshCw)
                                    .small()
                                    .label(t!("git.push_refresh_preview"))
                                    .disabled(self.loading || self.pushing)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if this.remotes.is_some() {
                                            this.request_preview(false, window, cx);
                                        } else {
                                            this.load_options(window, cx);
                                        }
                                    })),
                            )
                        }),
                )
            })
            .child(div().min_h(MIN_PREVIEW_HEIGHT).when_some(
                self.preview.as_ref(),
                |region, (_, preview)| {
                    region
                        .opacity(if stale { 0.62 } else { 1.0 })
                        .child(self.preview_content(preview, window, colors, cx))
                },
            ))
            .child(
                h_flex()
                    .id("git-push-footer")
                    .debug_selector(|| "git-push-footer".into())
                    .gap_2()
                    .justify_between()
                    .pt_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("git-push-upstream-field")
                                    .debug_selector(|| "git-push-upstream-field".into())
                                    .child(
                                        Checkbox::new("git-push-upstream")
                                            .small()
                                            .label(t!("git.publish_sets_upstream").into_owned())
                                            .checked(self.set_upstream)
                                            .disabled(self.pushing || target.is_none())
                                            .on_change(cx.listener(
                                                |this, checked: &bool, _, cx| {
                                                    this.set_upstream = *checked;
                                                    cx.notify();
                                                },
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .id("git-push-force")
                                    .debug_selector(|| "git-push-force".into())
                                    .child(
                                        Checkbox::new("git-push-force-toggle")
                                            .small()
                                            .label(t!("git.push_force").into_owned())
                                            .tooltip(t!("git.push_force_help"))
                                            .checked(force)
                                            .disabled(self.pushing || !can_force)
                                            .on_change(cx.listener(
                                                |this, checked: &bool, _, cx| {
                                                    this.force = *checked;
                                                    cx.notify();
                                                },
                                            )),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                action_button("git-push-cancel")
                                    .debug_selector(|| "git-push-cancel".into())
                                    .small()
                                    .secondary()
                                    .label(t!("common.cancel"))
                                    .disabled(self.pushing)
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(
                                action_button("git-push-apply")
                                    .debug_selector(|| "git-push-apply".into())
                                    .small()
                                    .when(force, |button| button.danger())
                                    .when(!force, |button| button.primary())
                                    .min_w(px(88.))
                                    .label(if force {
                                        t!("git.push_force")
                                    } else {
                                        t!("git.push")
                                    })
                                    .loading(self.pushing)
                                    .disabled(self.operation(cx).is_none())
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.run(window, cx)),
                                    ),
                            ),
                    ),
            )
    }
}
