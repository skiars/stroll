use std::{cell::Cell, rc::Rc};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    Placement,
    popover::Popover,
    tooltip::{ManagedTooltipExt as _, Positioner, TooltipRequest},
};
use rust_i18n::t;

use super::{CommitRow, Palette, file_count, history_rows::References};

const MARGIN: Pixels = px(8.);
const GAP: Pixels = px(10.);
const WIDTH: Pixels = px(340.);
const ARROW_INSET: Pixels = px(18.);

pub(super) fn preview_body(body: String) -> SharedString {
    const MAX_BYTES: usize = 2048;
    let end = body.floor_char_boundary(MAX_BYTES);
    if end == body.len() {
        body.into()
    } else {
        format!("{}…", body[..end].trim_end()).into()
    }
}

pub(super) fn attach(
    row: Stateful<Div>,
    title_y: Rc<Cell<Pixels>>,
    colors: Palette,
    content: impl Fn() -> PreviewContent + 'static,
) -> Stateful<Div> {
    row.managed_tooltip_request(true, move |row, window, _| {
        let target = point(row.right(), title_y.get().clamp(row.top(), row.bottom()));
        available_width(Positioner::viewport_bounds(window, MARGIN), target.x)?;
        let position = point(target.x + GAP, target.y - ARROW_INSET);
        let content = content();
        Some(
            TooltipRequest::new(row, move |_, cx| {
                cx.new(|_| Preview {
                    content: content.clone(),
                    colors,
                    target,
                })
                .into()
            })
            .with_positioner(move || Positioner::corner(Anchor::TopLeft, position).margin(MARGIN))
            .paint_only(),
        )
    })
}

fn available_width(viewport: Bounds<Pixels>, row_right: Pixels) -> Option<Pixels> {
    let width = (viewport.right() - row_right - GAP - px(1.)).min(WIDTH);
    (width >= px(160.) && viewport.size.height >= px(164.)).then_some(width)
}

#[derive(Clone)]
pub(super) struct PreviewContent {
    subject: SharedString,
    details: SharedString,
    labels: SharedString,
    body: SharedString,
}

impl PreviewContent {
    pub(super) fn worktree(
        subject: SharedString,
        count: Option<(usize, (usize, usize, usize))>,
    ) -> Self {
        let details = count.map(|(total, (staged, unstaged, untracked))| {
            t!(
                "git.worktree_count",
                total = file_count(total),
                staged = staged,
                unstaged = unstaged,
                untracked = untracked
            )
            .into_owned()
        });
        Self {
            subject,
            details: details.unwrap_or_default().into(),
            labels: SharedString::default(),
            body: SharedString::default(),
        }
    }

    pub(super) fn commit(commit: &CommitRow, references: &References) -> Self {
        let subject = if commit.subject.is_empty() {
            t!("git.empty_subject").into()
        } else {
            commit.subject.clone()
        };
        let references = references
            .get(commit.oid.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut labels = references
            .iter()
            .take(3)
            .map(|reference| reference.label.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        if references.len() > 3 {
            labels.push_str(&format!(" · +{}", references.len() - 3));
        }
        let time = super::review::display_date(&commit.committed_at);
        let details = if commit.merge {
            format!("{} · {time} · {}", commit.author, t!("git.merge"))
        } else {
            format!("{} · {time}", commit.author)
        };
        Self {
            subject,
            details: details.into(),
            labels: labels.into(),
            body: commit.body_preview.clone(),
        }
    }
}

struct Preview {
    content: PreviewContent,
    colors: Palette,
    target: Point<Pixels>,
}

impl Render for Preview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = Positioner::viewport_bounds(window, MARGIN);
        let Some(width) = available_width(viewport, self.target.x) else {
            return div().into_any_element();
        };
        let body_lines = (((viewport.size.height - px(118.)).as_f32() / 18.) as usize).clamp(2, 8);
        let PreviewContent {
            subject,
            details,
            labels,
            body,
        } = &self.content;
        let colors = self.colors;
        Popover::surface_with_arrow(Placement::Right, self.target, window, cx)
            .id("git-history-preview")
            .role(gpui::Role::Tooltip)
            .relative()
            .w(width)
            .px(px(13.))
            .py(px(10.))
            .gap(px(5.))
            .text_size(super::GIT_TEXT_SIZE)
            .line_height(px(18.))
            .child(
                div()
                    .w_full()
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_clamp(3)
                    .text_ellipsis()
                    .child(subject.clone()),
            )
            .when(!details.is_empty(), |card| {
                card.child(
                    div()
                        .w_full()
                        .truncate()
                        .text_color(colors.muted)
                        .child(details.clone()),
                )
            })
            .when(!labels.is_empty(), |card| {
                card.child(
                    div()
                        .w_full()
                        .truncate()
                        .text_color(colors.muted)
                        .child(labels.clone()),
                )
            })
            .when(!body.is_empty(), |card| {
                card.child(
                    div()
                        .w_full()
                        .pt(px(3.))
                        .line_clamp(body_lines)
                        .text_ellipsis()
                        .child(body.clone()),
                )
            })
            .into_any_element()
    }
}
