use std::cell::{OnceCell, RefCell};

use gpui_component::{
    IndexPath, WindowExt as _,
    menu::{DropdownMenu, PopupMenuItem},
    select::{Select, SelectDelegate, SelectItem, SelectState},
};

use super::*;

fn comparison_label(base: Option<&str>, target: Option<&str>) -> Cow<'static, str> {
    let base = base
        .map(git::short)
        .map(str::to_owned)
        .unwrap_or_else(|| t!("git.empty_tree").into_owned());
    let target = target
        .map(git::short)
        .map(str::to_owned)
        .unwrap_or_else(|| t!("git.current_worktree").into_owned());
    t!("git.compare_versions_label", base = base, target = target)
}

#[derive(Clone, Eq, PartialEq)]
struct RevisionChoice {
    oid: Option<String>,
    title: SharedString,
}

impl RevisionChoice {
    fn commit(oid: impl Into<String>, short: &str, subject: &str) -> Self {
        let title = if subject.is_empty() {
            short.into()
        } else {
            format!("{short}  {subject}").into()
        };
        Self {
            oid: Some(oid.into()),
            title,
        }
    }
}

impl SelectItem for RevisionChoice {
    type Value = Self;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self {
        self
    }

    fn matches(&self, query: &str) -> bool {
        self.title.to_lowercase().contains(query)
            || self.oid.as_ref().is_some_and(|oid| oid.starts_with(query))
    }
}

struct RevisionChoices {
    initial: Vec<RevisionChoice>,
    local: Option<Vec<RevisionChoice>>,
    matches: Rc<OnceCell<Vec<RevisionChoice>>>,
    error: Rc<RefCell<Option<String>>>,
    backend: git::Backend,
}

impl RevisionChoices {
    fn items(&self) -> &[RevisionChoice] {
        self.matches
            .get()
            .or(self.local.as_ref())
            .unwrap_or(&self.initial)
    }
}

impl SelectDelegate for RevisionChoices {
    type Item = RevisionChoice;

    fn items_count(&self, _: usize) -> usize {
        self.items().len()
    }

    fn item(&self, index: IndexPath) -> Option<&Self::Item> {
        self.items().get(index.row)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SelectItem<Value = V>,
        V: PartialEq,
    {
        self.items()
            .iter()
            .position(|choice| choice.value() == value)
            .map(IndexPath::new)
    }

    fn perform_search(&mut self, query: &str, _: &mut Window, cx: &mut App) -> Task<()> {
        self.matches = Rc::default();
        self.error.borrow_mut().take();
        let query = query.trim();
        if query.is_empty() {
            self.local = None;
            return Task::ready(());
        }
        let search = query.to_lowercase();
        let local = self
            .initial
            .iter()
            .filter(|choice| choice.matches(&search))
            .cloned()
            .collect::<Vec<_>>();
        self.local = Some(local.clone());
        let query = query.to_owned();
        let backend = self.backend.clone();
        let matches = self.matches.clone();
        let error = self.error.clone();
        cx.spawn(async move |cx| {
            let executor = cx.background_executor();
            executor.timer(Duration::from_millis(200)).await;
            match executor
                .spawn(async move { backend.search_revisions(&query) })
                .await
            {
                Ok(found) => {
                    let mut choices = local;
                    for item in found {
                        let choice = RevisionChoice::commit(item.oid, &item.short, &item.subject);
                        if !choices.contains(&choice) {
                            choices.push(choice);
                        }
                    }
                    _ = matches.set(choices);
                }
                Err(reason) => {
                    *error.borrow_mut() = Some(reason.to_string());
                }
            }
        })
    }
}

struct RevisionDialog {
    panel: WeakEntity<GitPanel>,
    backend: git::Backend,
    previous: Selection,
    base: Entity<SelectState<RevisionChoices>>,
    target: Entity<SelectState<RevisionChoices>>,
    search_errors: [Rc<RefCell<Option<String>>>; 2],
    error: Option<String>,
    apply: Option<Task<()>>,
}

impl RevisionDialog {
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.apply.is_some() {
            return;
        }
        let (Some(base), Some(target)) = (
            self.base.read(cx).selected_value(),
            self.target.read(cx).selected_value(),
        ) else {
            return;
        };
        let selection = Selection::Compare {
            base: base.oid.clone(),
            target: target.oid.clone(),
            previous: Box::new(self.previous.clone()),
        };
        let backend = self.backend.clone();
        let check = cx.background_spawn(async move { backend.check_revision_comparison() });
        self.error = None;
        self.apply = Some(cx.spawn_in(window, async move |this, cx| {
            let result = check.await;
            _ = this.update_in(cx, move |this, window, cx| {
                this.apply = None;
                match result {
                    Ok(()) => {
                        window.close_dialog(cx);
                        _ = this.panel.update(cx, |panel, cx| {
                            panel.load_review(selection, false, window, cx);
                        });
                    }
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for RevisionDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fields = [
            ("git-comparison-base", t!("git.comparison_base"), &self.base),
            (
                "git-comparison-target",
                t!("git.comparison_target"),
                &self.target,
            ),
        ];
        let font_size = cx.theme().control_font_size;
        let colors = Palette::for_mode(cx.theme().mode);
        v_flex()
            .gap_3()
            .children(fields.into_iter().zip(&self.search_errors).map(
                |((id, label, state), error)| {
                    let error = error.clone();
                    v_flex()
                        .gap_1()
                        .child(div().text_size(font_size).child(label.clone()))
                        .child(
                            div().debug_selector(move || id.into()).child(
                                Select::new(state)
                                    .id(id)
                                    .w_full()
                                    .disabled(self.apply.is_some())
                                    .accessibility_label(label)
                                    .search_placeholder(t!("git.search_versions"))
                                    .empty(move |_, _| {
                                        div().p_2().text_size(font_size).child(
                                            error.borrow().clone().unwrap_or_else(|| {
                                                t!("git.no_matching_commits").into_owned()
                                            }),
                                        )
                                    }),
                            ),
                        )
                },
            ))
            .when_some(self.error.clone(), |dialog, error| {
                dialog.child(message(error, colors.warning))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        action_button("git-cancel-comparison")
                            .secondary()
                            .label(t!("common.cancel"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply = None;
                                window.close_dialog(cx);
                            })),
                    )
                    .child(
                        action_button("git-apply-comparison")
                            .debug_selector(|| "git-apply-comparison".into())
                            .label(t!("git.compare"))
                            .disabled(self.apply.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                    ),
            )
    }
}

fn custom_item(label: impl Into<SharedString>, panel: WeakEntity<GitPanel>) -> PopupMenuItem {
    PopupMenuItem::new(label).on_click(move |_, window, cx| {
        _ = panel.update(cx, |this, cx| this.open_revision_dialog(window, cx));
    })
}

impl GitPanel {
    pub(super) fn compare_with_base(
        &mut self,
        base: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = &self.selection else {
            return;
        };
        let previous = match selection {
            Selection::Compare { previous, .. } => previous.clone(),
            _ => Box::new(selection.clone()),
        };
        self.load_review(
            Selection::Compare {
                base: Some(base.to_owned()),
                target: selection.target_commit().map(str::to_owned),
                previous,
            },
            false,
            window,
            cx,
        );
    }

    pub(super) fn comparison_control(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let weak = cx.weak_entity();
        let versions = match self.selection.as_ref()? {
            Selection::Compare {
                base,
                target,
                previous,
            } => Some((
                "git-custom-comparison",
                base.as_deref(),
                target.as_deref(),
                Some(previous.as_ref().clone()),
            )),
            Selection::CommitRange { base, newest, .. } => Some((
                "git-commit-range-comparison",
                base.as_deref(),
                Some(newest.as_str()),
                None,
            )),
            _ => None,
        };
        if let Some((id, base, target, previous)) = versions {
            let label = comparison_label(base, target);
            return Some(
                action_button(id)
                    .debug_selector(move || id.into())
                    .secondary()
                    .min_w_0()
                    .label(label.clone())
                    .tooltip(label)
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu, _, _| {
                        let view = weak.clone();
                        menu.item(custom_item(t!("git.custom_comparison"), weak.clone()))
                            .when_some(previous.clone(), |menu, previous| {
                                menu.item(
                                    PopupMenuItem::new(t!("git.return_to_comparison")).on_click(
                                        move |_, window, cx| {
                                            _ = view.update(cx, |this, cx| {
                                                this.load_review(
                                                    previous.clone(),
                                                    false,
                                                    window,
                                                    cx,
                                                )
                                            });
                                        },
                                    ),
                                )
                            })
                    })
                    .into_any_element(),
            );
        }

        let review = self.review.as_ref()?;
        let control = match &review.detail.comparison {
            Comparison::Commit { commit, parent } => {
                let label = parent.as_deref().map_or_else(
                    || t!("git.compare_empty_tree"),
                    |oid| t!("git.compare_parent", commit = git::short(oid)),
                );
                let parents = commit.parents.clone();
                let oid = commit.oid.clone();
                let current = parent.clone();
                action_button("git-select-parent")
                    .debug_selector(|| "git-select-parent".into())
                    .secondary()
                    .min_w_0()
                    .label(label.clone())
                    .tooltip(label)
                    .dropdown_caret(true)
                    .dropdown_menu(move |mut menu, _, _| {
                        if parents.len() > 1 {
                            for (ix, parent) in parents.iter().enumerate() {
                                let view = weak.clone();
                                let selection =
                                    Selection::Commit(oid.clone(), Some(parent.clone()));
                                let label = format!(
                                    "{}{}",
                                    git::short(parent),
                                    if ix == 0 {
                                        t!("git.first_parent_suffix")
                                    } else {
                                        Cow::Borrowed("")
                                    }
                                );
                                menu = menu.item(
                                    PopupMenuItem::new(label)
                                        .checked(current.as_ref() == Some(parent))
                                        .on_click(move |_, window, cx| {
                                            _ = view.update(cx, |this, cx| {
                                                this.load_review(
                                                    selection.clone(),
                                                    false,
                                                    window,
                                                    cx,
                                                )
                                            });
                                        }),
                                );
                            }
                            menu = menu.separator();
                        }
                        menu.item(custom_item(t!("git.custom_comparison"), weak.clone()))
                    })
            }
            Comparison::Worktree { mode, base } => {
                let current = *mode;
                let editable = base.is_none();
                let label = base.as_deref().map_or_else(
                    || mode.label(),
                    |oid| t!("git.to_worktree", base = git::short(oid)),
                );
                action_button("git-worktree-mode")
                    .debug_selector(|| "git-worktree-mode".into())
                    .secondary()
                    .min_w_0()
                    .label(label.clone())
                    .tooltip(label)
                    .dropdown_caret(true)
                    .accessibility_label(t!("git.comparison_mode"))
                    .dropdown_menu(move |mut menu, _, _| {
                        if editable {
                            for mode in [Mode::All, Mode::Staged, Mode::Unstaged] {
                                let view = weak.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(mode.label())
                                        .checked(mode == current)
                                        .on_click(move |_, window, cx| {
                                            _ = view.update(cx, |this, cx| {
                                                this.load_review(
                                                    Selection::Worktree(mode),
                                                    false,
                                                    window,
                                                    cx,
                                                )
                                            });
                                        }),
                                );
                            }
                            menu = menu.separator();
                        }
                        menu.item(custom_item(t!("git.compare_other_version"), weak.clone()))
                    })
            }
            Comparison::Versions { .. } => return None,
        };
        Some(control.into_any_element())
    }

    pub(super) fn open_revision_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = &self.selection else {
            return;
        };
        let (base, target, previous) = match selection {
            Selection::Compare {
                base,
                target,
                previous,
            } => (base.clone(), target.clone(), previous.as_ref().clone()),
            Selection::CommitRange { base, newest, .. } => {
                (base.clone(), Some(newest.clone()), selection.clone())
            }
            _ => {
                let Some(review) = &self.review else { return };
                let (base, target) = match &review.detail.comparison {
                    Comparison::Commit { commit, parent } => {
                        (parent.clone(), Some(commit.oid.clone()))
                    }
                    Comparison::Worktree { base, .. } => (
                        base.clone().or_else(|| self.history.as_ref()?.head.clone()),
                        None,
                    ),
                    Comparison::Versions { .. } => return,
                };
                (base, target, selection.clone())
            }
        };
        let mut choices = Vec::new();
        if let Some(history) = &self.history {
            choices.extend(history.branches.iter().map(|branch| RevisionChoice {
                oid: Some(branch.oid.clone()),
                title: branch.label.clone().into(),
            }));
            let mut tags = history
                .refs
                .iter()
                .flat_map(|(oid, refs)| {
                    refs.iter()
                        .filter(|reference| reference.tag)
                        .map(move |tag| (oid, &tag.label))
                })
                .collect::<Vec<_>>();
            tags.sort_unstable_by_key(|(_, label)| *label);
            choices.extend(tags.into_iter().map(|(oid, label)| RevisionChoice {
                oid: Some(oid.clone()),
                title: label.clone().into(),
            }));
        }
        choices.extend(
            self.commits
                .iter()
                .take(git::PAGE_SIZE)
                .map(|row| RevisionChoice::commit(row.oid.to_string(), &row.short, &row.subject)),
        );
        let worktree = RevisionChoice {
            oid: None,
            title: t!("git.current_worktree").into(),
        };
        let make_choices =
            |oid: Option<String>, empty: RevisionChoice, mut choices: Vec<RevisionChoice>| {
                let current = oid.map_or_else(
                    || empty.clone(),
                    |oid| {
                        self.commits
                            .iter()
                            .find(|row| row.oid.as_str() == oid)
                            .map_or_else(
                                || RevisionChoice::commit(oid.clone(), git::short(&oid), ""),
                                |row| RevisionChoice::commit(oid.clone(), &row.short, &row.subject),
                            )
                    },
                );
                choices.retain(|choice| choice != &current);
                if current.oid.is_some() && empty == worktree {
                    choices.insert(0, worktree.clone());
                }
                choices.insert(0, current);
                RevisionChoices {
                    initial: choices,
                    local: None,
                    matches: Rc::default(),
                    error: Rc::default(),
                    backend: self.backend.clone(),
                }
            };
        let base = make_choices(
            base,
            RevisionChoice {
                oid: None,
                title: t!("git.empty_tree").into(),
            },
            choices.clone(),
        );
        let target = make_choices(target, worktree.clone(), choices);
        let search_errors = [base.error.clone(), target.error.clone()];
        let base = cx
            .new(|cx| SelectState::new(base, Some(IndexPath::new(0)), window, cx).searchable(true));
        let target = cx.new(|cx| {
            SelectState::new(target, Some(IndexPath::new(0)), window, cx).searchable(true)
        });
        let panel = cx.weak_entity();
        let dialog = cx.new(|_| RevisionDialog {
            panel,
            backend: self.backend.clone(),
            previous,
            base,
            target,
            search_errors,
            error: None,
            apply: None,
        });
        window.open_dialog(cx, move |surface, _, _| {
            surface
                .centered()
                .title(t!("git.compare_versions"))
                .width(px(460.))
                .child(dialog.clone())
                .on_ok(|_, _, _| false)
                .on_close({
                    let dialog = dialog.clone();
                    move |_, _, cx| dialog.update(cx, |dialog, _| dialog.apply = None)
                })
        });
    }
}
