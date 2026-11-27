use super::*;
use gpui_component::input::Rope;

#[derive(Clone)]
pub(in crate::workspace) struct HistorySelection {
    pub rows: (usize, usize),
    pub saved: Rope,
    pub draft: Rope,
}

#[derive(Clone)]
pub(super) struct SourceHistoryRequest {
    pub(super) path: String,
    selection: Option<HistorySelection>,
}

#[derive(Clone)]
pub(super) struct PreparedHistory {
    pub(super) target: Option<git::SourceHistoryTarget>,
    skipped: Vec<(usize, usize)>,
}

pub(super) struct SourceHistory {
    pub(super) request: SourceHistoryRequest,
    pub(super) prepared: Option<PreparedHistory>,
    previous: Option<HistoryRestore>,
}

impl SourceHistoryRequest {
    pub(super) fn prepare(&self, backend: &git::Backend) -> Result<PreparedHistory> {
        backend.check_source_history_support()?;
        let version = backend.file_version(&self.path)?;
        let path = version
            .blame_path(&self.path)
            .unwrap_or(&self.path)
            .to_owned();
        let tracked = !version
            .status
            .as_ref()
            .is_some_and(|status| status.untracked);
        let head = version.head.filter(|_| tracked);

        let (ranges, skipped) = if let Some(selection) = &self.selection {
            let baseline = match &head {
                Some(head) => backend.blame_baseline(&path, head)?,
                None => None,
            };
            match baseline {
                Some(baseline) => super::super::editor_git::blame::map_history_selection(
                    &baseline,
                    &selection.saved,
                    &selection.draft,
                    selection.rows,
                ),
                None => (Vec::new(), vec![selection.rows]),
            }
        } else {
            (Vec::new(), Vec::new())
        };
        let target = head
            .filter(|_| self.selection.is_none() || !ranges.is_empty())
            .map(|head| git::SourceHistoryTarget { head, path, ranges });
        Ok(PreparedHistory { target, skipped })
    }
}

impl GitPanel {
    pub(in crate::workspace) fn open_source_history(
        &mut self,
        path: String,
        selection: Option<HistorySelection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = match self.source_history.take() {
            Some(history) => history.previous,
            None => self.restore_history.take().or_else(|| {
                self.history_view_position()
                    .map(|position| HistoryRestore::Session { position, pages: 0 })
            }),
        };
        if let Some(session) = &mut self.startup_session {
            session.selection = None;
            session.diff = None;
        }
        self.query_task = Task::ready(());
        self.clear_history_selection(window, cx);
        self.source_history = Some(SourceHistory {
            request: SourceHistoryRequest { path, selection },
            prepared: None,
            previous,
        });
        self.reset_history_rows();
        self.set_history_visible(true, cx);
        self.load_commits(false, window, cx);
        self.history_focus.focus(window, cx);
    }

    fn clear_source_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(history) = self.source_history.take() else {
            return;
        };
        self.restore_history = history.previous;
        self.reset_history_rows();
        self.load_commits(false, window, cx);
        self.history_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn reset_history_rows(&mut self) {
        self.history_scroll.reset(0);
        self.commits = Rc::default();
        self.graph = Rc::default();
    }

    pub(super) fn source_history_header(
        &self,
        scope: &SourceHistory,
        colors: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let label: SharedString = match &scope.request.selection {
            Some(selection) => t!(
                "git.source_history_lines",
                path = scope.request.path,
                lines = line_range(selection.rows),
            )
            .into(),
            None => scope.request.path.clone().into(),
        };
        let tooltip = label.clone();
        v_flex()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .id("git-source-history-scope")
                            .debug_selector(|| "git-source-history-scope".into())
                            .min_w_0()
                            .flex_1()
                            .text_size(GIT_TEXT_SIZE)
                            .truncate()
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip.clone()).build(window, cx)
                            })
                            .child(label),
                    )
                    .child(
                        icon_button(
                            "git-clear-source-history",
                            IconName::X,
                            t!("git.clear_source_history"),
                        )
                        .debug_selector(|| "git-clear-source-history".into())
                        .on_click(cx.listener(|panel, _, window, cx| {
                            panel.clear_source_history(window, cx);
                        })),
                    ),
            )
            .when_some(
                scope
                    .prepared
                    .as_ref()
                    .filter(|prepared| prepared.target.is_some() && !prepared.skipped.is_empty()),
                |header, prepared| {
                    let lines: SharedString = prepared
                        .skipped
                        .iter()
                        .copied()
                        .map(line_range)
                        .collect::<Vec<_>>()
                        .join(", ")
                        .into();
                    let label: SharedString =
                        t!("git.source_history_skipped_lines", lines = lines).into();
                    header.child(
                        div()
                            .id("git-source-history-skipped")
                            .debug_selector(|| "git-source-history-skipped".into())
                            .text_size(GIT_BADGE_SIZE)
                            .text_color(colors.muted)
                            .truncate()
                            .tooltip({
                                let label = label.clone();
                                move |window, cx| Tooltip::new(label.clone()).build(window, cx)
                            })
                            .child(label),
                    )
                },
            )
    }

    pub(super) fn source_history_empty(&self) -> Cow<'static, str> {
        if self.source_history.as_ref().is_some_and(|scope| {
            scope.request.selection.is_some()
                && scope
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| prepared.target.is_none())
        }) {
            t!("git.no_committed_lines")
        } else {
            t!("git.no_source_history")
        }
    }

    pub(super) fn source_history_position(&self, oid: &str) -> Option<DiffPosition> {
        let row = self.commits.iter().find(|row| row.oid.as_str() == oid)?;
        Some(DiffPosition {
            path: row.source_path.clone()?,
            line: None,
            inset: 0.,
            horizontal: 0.,
        })
    }
}

fn line_range((first, last): (usize, usize)) -> String {
    if first == last {
        first.to_string()
    } else {
        format!("{first}–{last}")
    }
}
