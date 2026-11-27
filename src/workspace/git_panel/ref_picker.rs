use gpui_component::{
    Icon, IndexPath, Selectable, Size as ControlSize, StyleSized as _, ThemeMode,
    list::{List, ListDelegate, ListState as PickerList},
    popover::Popover,
};

use super::*;
use crate::{
    paths,
    repository::Worktree,
    workspace::hover_info::{BranchHover, Info},
};

const ROW_HEIGHT: Pixels = px(29.);
const RECENT_LIMIT: usize = 32;
const SYNC_ARROW: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><path d="M5 8V2M2.7 4.3 5 2l2.3 2.3" fill="none" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

pub(super) fn field_background(cx: &App) -> Hsla {
    cx.theme().background.alpha(0.48)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Local,
    Remote,
    Tags,
}

impl Group {
    const ALL: [Self; 3] = [Self::Local, Self::Remote, Self::Tags];

    fn label(self) -> SharedString {
        match self {
            Self::Local => t!("git.local_branches"),
            Self::Remote => t!("git.remote_branches"),
            Self::Tags => t!("git.tags"),
        }
        .into()
    }

    fn icon(self) -> IconName {
        match self {
            Self::Local => IconName::GitBranch,
            Self::Remote => IconName::Cloud,
            Self::Tags => IconName::Tag,
        }
    }
}

struct RefChoice {
    reference: String,
    name: SharedString,
    group: Group,
    current: bool,
    tracking: Option<git::Tracking>,
    oid: Option<String>,
}

impl RefChoice {
    fn hover_info(&self, worktree: Option<&Worktree>) -> Info {
        let mut info = Info::new(self.name.clone());
        info.badge = if self.current {
            Some(t!("git.current_worktree").into())
        } else {
            worktree.map(|tree| worktree_label(tree.main).into())
        };
        info.commit(self.oid.as_deref());
        if let Some(tree) = worktree {
            info.row(t!("hover.worktree_path"), paths::display(&tree.root));
        }
        if let Some(tracking) = &self.tracking {
            info.row(
                t!("hover.upstream"),
                branch_name(&tracking.reference).to_owned(),
            );
            if tracking.gone {
                info.row(t!("hover.tracking"), t!("git.upstream_gone"));
            } else {
                info.row(
                    t!("hover.tracking"),
                    format!(
                        "{} · {}",
                        t!("git.ref_ahead", count = tracking.ahead),
                        t!("git.ref_behind", count = tracking.behind),
                    ),
                );
            }
        }
        if self.reference != "HEAD" && self.group != Group::Tags {
            info.branch_to_copy = Some(self.name.clone());
        }
        info
    }

    fn tooltip(&self, worktree: Option<bool>) -> SharedString {
        let mut details = vec![self.name.to_string()];
        if self.current {
            details.push(t!("git.current_worktree").into_owned());
        } else if let Some(main) = worktree {
            details.push(worktree_label(main).into_owned());
        }
        if let Some(tracking) = &self.tracking {
            details.push(
                t!(
                    "git.ref_upstream",
                    upstream = branch_name(&tracking.reference)
                )
                .into_owned(),
            );
            if tracking.gone {
                details.push(t!("git.upstream_gone").into_owned());
            } else {
                if tracking.behind > 0 {
                    details.push(t!("git.ref_behind", count = tracking.behind).into_owned());
                }
                if tracking.ahead > 0 {
                    details.push(t!("git.ref_ahead", count = tracking.ahead).into_owned());
                }
            }
        }
        details.join(" · ").into()
    }
}

fn worktree_label(main: bool) -> Cow<'static, str> {
    if main {
        t!("git.main_worktree")
    } else {
        t!("git.worktree_badge")
    }
}

fn sync_status(tracking: &git::Tracking, colors: Palette, mode: ThemeMode) -> Div {
    let ahead_color: Hsla = rgb(if mode.is_dark() { 0x93dcc6 } else { 0x146953 }).into();
    let count = |value: usize, down: bool, color: Hsla| {
        h_flex()
            .gap(px(1.))
            .text_color(color)
            .child(
                Icon::default()
                    .data(SYNC_ARROW)
                    .with_size(px(10.))
                    .text_color(color)
                    .when(down, |arrow| arrow.rotate(radians(std::f32::consts::PI))),
            )
            .child(value.to_string())
    };
    h_flex()
        .flex_shrink_0()
        .gap(px(4.))
        .text_size(GIT_BADGE_SIZE)
        .when(tracking.gone, |row| {
            row.child(icon(IconName::Unlink2, colors.warning, 11.))
        })
        .when(!tracking.gone && tracking.behind > 0, |row| {
            row.child(count(tracking.behind, true, colors.warning))
        })
        .when(!tracking.gone && tracking.ahead > 0, |row| {
            row.child(count(tracking.ahead, false, ahead_color))
        })
}

#[derive(Clone, Copy)]
enum Row {
    Ref(usize),
    Toggle {
        group: Group,
        count: usize,
        expanded: bool,
    },
}

struct Section {
    group: Group,
    rows: Vec<Row>,
}

struct RefList {
    panel: WeakEntity<GitPanel>,
    refs: Vec<RefChoice>,
    sections: Vec<Section>,
    query: String,
    selected: String,
    cursor: Option<IndexPath>,
    expanded: [bool; 2],
    worktrees: HashMap<String, Worktree>,
    branch_hover: BranchHover,
}

impl RefList {
    fn rebuild(&mut self) {
        let needle = self.query.to_lowercase();
        let mut sections: Vec<Section> = Vec::new();
        for group in Group::ALL {
            let rows = self
                .refs
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    item.group == group
                        && (needle.is_empty()
                            || item.name.to_lowercase().contains(&needle)
                            || item.tracking.as_ref().is_some_and(|tracking| {
                                branch_name(&tracking.reference)
                                    .to_lowercase()
                                    .contains(&needle)
                            }))
                })
                .map(|(index, _)| Row::Ref(index))
                .collect::<Vec<_>>();
            if rows.is_empty() {
                continue;
            }
            if let (Some(section), Some(expanded)) = (
                sections.first_mut(),
                self.query.is_empty().then(|| self.is_expanded(group)),
            ) {
                section.rows.push(Row::Toggle {
                    group,
                    count: rows.len(),
                    expanded,
                });
                if expanded {
                    section.rows.extend(rows);
                }
            } else {
                sections.push(Section { group, rows });
            }
        }
        self.sections = sections;
    }

    fn is_expanded(&self, group: Group) -> bool {
        match group {
            Group::Local => true,
            Group::Remote => self.expanded[0],
            Group::Tags => self.expanded[1],
        }
    }

    fn toggle(&mut self, group: Group) {
        match group {
            Group::Remote => self.expanded[0] = !self.expanded[0],
            Group::Tags => self.expanded[1] = !self.expanded[1],
            Group::Local => return,
        }
        self.rebuild();
    }

    fn row(&self, ix: IndexPath) -> Option<Row> {
        self.sections.get(ix.section)?.rows.get(ix.row).copied()
    }

    fn highlighted(&self, ix: Option<IndexPath>) -> Option<&str> {
        match self.row(ix?)? {
            Row::Ref(index) => Some(&self.refs[index].reference),
            Row::Toggle { .. } => None,
        }
    }

    fn position(&self, reference: &str) -> Option<IndexPath> {
        self.sections
            .iter()
            .enumerate()
            .find_map(|(section, group)| {
                group.rows.iter().position(|row| {
                matches!(row, Row::Ref(index) if self.refs[*index].reference == reference)
            }).map(|row| IndexPath::default().section(section).row(row))
            })
    }
}

impl ListDelegate for RefList {
    type Item = RefRow;

    fn sections_count(&self, _: &App) -> usize {
        self.sections.len()
    }

    fn items_count(&self, section: usize, _: &App) -> usize {
        self.sections
            .get(section)
            .map_or(0, |section| section.rows.len())
    }

    fn render_section_header(
        &mut self,
        section: usize,
        _: &mut Window,
        cx: &mut Context<PickerList<Self>>,
    ) -> Option<impl IntoElement> {
        Some(
            h_flex()
                .h(px(24.))
                .px(px(8.))
                .text_size(GIT_BADGE_SIZE)
                .text_color(Palette::for_mode(cx.theme().mode).muted)
                .child(self.sections.get(section)?.group.label()),
        )
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<PickerList<Self>>,
    ) -> Option<RefRow> {
        let mode = cx.theme().mode;
        let colors = Palette::for_mode(mode);
        let row = match self.row(ix)? {
            Row::Ref(index) => {
                let choice = &self.refs[index];
                let name = choice.reference.clone();
                let worktree = self.worktrees.get(&name);
                let label = choice.tooltip(worktree.map(|tree| tree.main));
                let badge_selector = format!("git-ref-worktree:{name}");
                RefRow {
                    id: SharedString::from(format!("git-ref:{name}")).into(),
                    selector: format!("git-ref:{name}"),
                    label: label.clone(),
                    tooltip: Some((choice.hover_info(worktree), self.branch_hover.clone())),
                    expanded: None,
                    committed: name == self.selected,
                    keyboard: false,
                    body: h_flex()
                        .size_full()
                        .gap(px(6.))
                        .child(icon(choice.group.icon(), colors.muted, 13.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(choice.name.clone()),
                        )
                        .when(choice.current, |row| {
                            row.child(icon(IconName::Star, colors.warning, 13.))
                        })
                        .when(worktree.is_some() && !choice.current, |row| {
                            row.child(
                                div()
                                    .flex_shrink_0()
                                    .debug_selector(move || badge_selector.clone())
                                    .child(icon(IconName::FolderGit2, colors.muted, 13.)),
                            )
                        })
                        .when_some(choice.tracking.as_ref(), |row, tracking| {
                            row.child(sync_status(tracking, colors, mode))
                        })
                        .into_any_element(),
                }
            }
            Row::Toggle {
                group,
                count,
                expanded,
            } => {
                let id = match group {
                    Group::Remote => "git-ref-remote",
                    Group::Tags => "git-ref-tags",
                    Group::Local => unreachable!(),
                };
                let label = group.label();
                RefRow {
                    id: id.into(),
                    selector: id.into(),
                    label: format!("{label} ({count})").into(),
                    tooltip: None,
                    expanded: Some(expanded),
                    committed: false,
                    keyboard: false,
                    body: h_flex()
                        .size_full()
                        .gap(px(6.))
                        .text_size(GIT_BADGE_SIZE)
                        .text_color(colors.muted)
                        .child(icon(
                            if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                            colors.muted,
                            12.,
                        ))
                        .child(div().flex_1().child(label))
                        .child(count.to_string())
                        .into_any_element(),
                }
            }
        };
        Some(row)
    }

    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<PickerList<Self>>,
    ) -> Task<()> {
        self.query = query.to_owned();
        self.rebuild();
        if !query.is_empty() && !self.sections.is_empty() {
            cx.defer_in(window, |list, window, cx| {
                if list.selected_index().is_none()
                    && !list.delegate().query.is_empty()
                    && !list.delegate().sections.is_empty()
                {
                    list.set_selected_index(Some(IndexPath::default()), window, cx);
                    cx.notify();
                }
            });
        }
        cx.notify();
        Task::ready(())
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<PickerList<Self>>,
    ) -> impl IntoElement {
        div()
            .px_2()
            .py_2()
            .text_color(Palette::for_mode(cx.theme().mode).muted)
            .child(t!("git.no_matching_refs"))
    }

    fn set_selected_index(
        &mut self,
        cursor: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<PickerList<Self>>,
    ) {
        self.cursor = cursor;
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<PickerList<Self>>) {
        let Some(row) = self.cursor.and_then(|index| self.row(index)) else {
            return;
        };
        match row {
            Row::Toggle { group, .. } => self.toggle(group),
            Row::Ref(index) => {
                let reference = self.refs[index].reference.clone();
                let panel = self.panel.clone();
                window.defer(cx, move |window, cx| {
                    _ = panel.update(cx, |panel, cx| {
                        panel.ref_picker.open = false;
                        panel.ref_picker.trigger.focus(window, cx);
                        if panel
                            .history
                            .as_ref()
                            .is_some_and(|history| history.has_browsable_ref(&reference))
                        {
                            panel.select_branch(reference, window, cx);
                        }
                        cx.notify();
                    });
                });
            }
        }
    }
}

#[derive(IntoElement)]
struct RefRow {
    id: ElementId,
    selector: String,
    label: SharedString,
    tooltip: Option<(Info, BranchHover)>,
    expanded: Option<bool>,
    committed: bool,
    keyboard: bool,
    body: AnyElement,
}

impl Selectable for RefRow {
    fn selected(mut self, selected: bool) -> Self {
        self.keyboard = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.keyboard
    }
}

impl RenderOnce for RefRow {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = Palette::for_mode(cx.theme().mode);
        div()
            .id(self.id)
            .debug_selector(move || self.selector.clone())
            .w_full()
            .h(ROW_HEIGHT)
            .px(px(7.))
            .rounded(px(5.))
            .text_size(cx.theme().control_font_size)
            .text_color(colors.ink)
            .aria_label(self.label)
            .cursor_pointer()
            .when(self.committed || self.keyboard, |row| {
                row.bg(colors.accent.alpha(if self.keyboard { 0.18 } else { 0.1 }))
            })
            .when(!self.committed && !self.keyboard, |row| {
                row.hover(|style| style.bg(colors.ink.alpha(0.07)))
            })
            .when(self.committed, |row| row.font_semibold())
            .when_some(self.expanded, |row, expanded| row.aria_expanded(expanded))
            .when_some(self.tooltip, |row, (info, hover)| info.attach(row, hover))
            .child(self.body)
    }
}

pub(super) struct RefPicker {
    list: Entity<PickerList<RefList>>,
    trigger: FocusHandle,
    open: bool,
    worktree_task: Task<()>,
    pub(super) recent: Vec<String>,
}

impl RefPicker {
    pub(super) fn new(
        recent: Vec<String>,
        window: &mut Window,
        cx: &mut Context<GitPanel>,
    ) -> Self {
        let panel = cx.weak_entity();
        Self {
            trigger: cx.focus_handle(),
            list: cx.new(|cx| {
                PickerList::new(
                    RefList {
                        panel,
                        refs: Vec::new(),
                        sections: Vec::new(),
                        query: String::new(),
                        selected: String::new(),
                        cursor: None,
                        expanded: [false; 2],
                        worktrees: HashMap::new(),
                        branch_hover: BranchHover::default(),
                    },
                    window,
                    cx,
                )
                .searchable(true)
            }),
            open: false,
            worktree_task: Task::ready(()),
            recent,
        }
    }

    pub(super) fn remember(&mut self, reference: &str) {
        if reference.starts_with("refs/heads/") {
            self.recent.retain(|item| item != reference);
            self.recent.insert(0, reference.to_owned());
            self.recent.truncate(RECENT_LIMIT);
        }
    }

    pub(super) fn update(
        &mut self,
        history: &History,
        selected: &str,
        window: &mut Window,
        cx: &mut Context<GitPanel>,
    ) {
        let checkout = history.checkout.as_deref().unwrap_or("HEAD");
        let mut refs = history
            .branches
            .iter()
            .map(|branch| RefChoice {
                reference: branch.reference.clone(),
                name: branch.label.clone().into(),
                group: if branch.remote {
                    Group::Remote
                } else {
                    Group::Local
                },
                current: branch.reference == checkout,
                tracking: branch.tracking.clone(),
                oid: Some(branch.oid.clone()),
            })
            .collect::<Vec<_>>();
        if !refs.iter().any(|item| item.current) {
            refs.push(RefChoice {
                reference: checkout.to_owned(),
                name: if checkout == "HEAD" {
                    t!("git.detached_head").into()
                } else {
                    branch_name(checkout).to_owned().into()
                },
                group: Group::Local,
                current: true,
                tracking: None,
                oid: history.head.clone(),
            });
        }
        let recent_rank = |reference: &str| {
            self.recent
                .iter()
                .position(|item| item == reference)
                .unwrap_or(RECENT_LIMIT)
        };
        refs.sort_by(|a, b| {
            (!a.current, recent_rank(&a.reference), a.name.as_ref()).cmp(&(
                !b.current,
                recent_rank(&b.reference),
                b.name.as_ref(),
            ))
        });
        let mut tags = history
            .refs
            .iter()
            .flat_map(|(oid, items)| items.iter().map(move |item| (item, oid)))
            .filter(|(item, _)| item.tag)
            .map(|(item, oid)| (&item.label, oid))
            .collect::<Vec<_>>();
        tags.sort_unstable();
        refs.extend(tags.into_iter().map(|(tag, oid)| RefChoice {
            reference: format!("refs/tags/{tag}"),
            name: tag.clone().into(),
            group: Group::Tags,
            current: false,
            tracking: None,
            oid: Some(oid.clone()),
        }));

        self.list.update(cx, |list, cx| {
            let highlight = list
                .delegate()
                .highlighted(list.selected_index())
                .map(str::to_owned);
            let delegate = list.delegate_mut();
            delegate.refs = refs;
            delegate.selected = selected.to_owned();
            delegate.rebuild();
            let ix = highlight
                .as_deref()
                .and_then(|item| delegate.position(item));
            list.set_selected_index(ix, window, cx);
            cx.notify();
        });
    }
}

impl GitPanel {
    fn set_ref_picker_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.ref_picker.open = open;
        if open {
            self.ref_picker.list.update(cx, |list, cx| {
                let delegate = list.delegate_mut();
                delegate.expanded = [
                    self.branch.starts_with("refs/remotes/"),
                    self.branch.starts_with("refs/tags/"),
                ];
                if delegate.query.is_empty() {
                    delegate.rebuild();
                } else {
                    list.set_query("", window, cx);
                }
                list.set_selected_index(None, window, cx);
                list.scroll_to_item(IndexPath::default(), ScrollStrategy::Top, window, cx);
                list.focus(window, cx);
                cx.notify();
            });
            let repo = self.repository.clone();
            let root = repo.root.clone();
            self.ref_picker.worktree_task = cx.spawn_in(window, async move |panel, cx| {
                let trees = cx.background_spawn(async move { repo.worktrees() }).await;
                if let Ok(trees) = trees {
                    let worktrees = trees
                        .into_iter()
                        .filter(|tree| tree.available)
                        .filter_map(|tree| {
                            let reference = tree
                                .branch
                                .clone()
                                .or_else(|| (tree.root == root).then(|| "HEAD".to_owned()));
                            reference.map(|reference| (reference, tree))
                        })
                        .collect();
                    _ = panel.update(cx, |panel, cx| {
                        panel.ref_picker.list.update(cx, |list, cx| {
                            list.delegate_mut().worktrees = worktrees;
                            cx.notify();
                        });
                    });
                }
            });
        }
        cx.notify();
    }

    pub(super) fn render_ref_picker(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement + use<> {
        let mode = cx.theme().mode;
        let list = &self.ref_picker.list;
        let choice = list
            .read(cx)
            .delegate()
            .refs
            .iter()
            .find(|item| item.reference == self.branch);
        let group = choice.map_or(Group::Local, |item| item.group);
        let name = choice.map_or_else(|| t!("git.select_branch").into(), |item| item.name.clone());
        let a11y = choice.map_or_else(
            || t!("git.browse_branches").into(),
            |item| item.tooltip(None),
        );
        let button = quiet_button("git-ref-picker-trigger")
            .w_full()
            .input_h(ControlSize::default())
            .border_1()
            .border_color(if self.ref_picker.open {
                colors.accent
            } else {
                cx.theme().input
            })
            .bg(field_background(cx))
            .px(px(7.))
            .track_focus(&self.ref_picker.trigger)
            .accessibility_label(a11y)
            .disabled(self.history.is_none())
            .child(
                h_flex()
                    .size_full()
                    .gap(px(6.))
                    .text_size(cx.theme().control_font_size)
                    .text_color(colors.ink)
                    .child(icon(group.icon(), colors.accent, 13.))
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .when_some(
                        choice.and_then(|item| item.tracking.as_ref()),
                        |row, tracking| row.child(sync_status(tracking, colors, mode)),
                    )
                    .child(icon(IconName::ChevronDown, colors.muted, 12.)),
            );
        let width = (window.viewport_size().width - px(16.)).clamp(px(0.), px(365.));
        let height = (window.viewport_size().height - px(162.)).clamp(px(70.), px(270.));
        let enabled = self.history.is_some();
        div()
            .id("git-branch-picker")
            .debug_selector(|| "git-branch-picker".into())
            .w_full()
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if enabled
                    && event.keystroke.key == "down"
                    && this.ref_picker.trigger.is_focused(window)
                {
                    cx.stop_propagation();
                    this.set_ref_picker_open(true, window, cx);
                }
            }))
            .child(if enabled {
                let branch_hover = list.read(cx).delegate().branch_hover.clone();
                let list = list.clone();
                Popover::new("git-ref-picker")
                    .overlay_closable(false)
                    .dropdown_motion()
                    .scrollable(false)
                    .offset(px(4.))
                    .p_0()
                    .w(width)
                    .open(self.ref_picker.open)
                    .trigger_style(div().w_full().style().clone())
                    .track_focus(&list.focus_handle(cx))
                    .on_open_change(cx.listener(|this, &open, window, cx| {
                        this.set_ref_picker_open(open, window, cx);
                    }))
                    .trigger(button)
                    .content(move |_, _, cx| {
                        let menu = div()
                            .id("git-ref-picker-content")
                            .debug_selector(|| "git-ref-picker-popup".into())
                            .child(
                                List::new(&list)
                                    .popup_scrollbar()
                                    .paddings(Edges::all(px(4.)))
                                    .max_h(height)
                                    .search_placeholder(t!("git.search_branches")),
                            );
                        branch_hover.dismiss_menu_on_outside(menu, cx.weak_entity())
                    })
                    .into_any_element()
            } else {
                button.into_any_element()
            })
    }
}
