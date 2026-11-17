use std::{cell::Cell, collections::HashMap, rc::Rc, sync::Arc};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{ElementExt as _, h_flex, v_flex};
use gpui_kit_assets::IconName;
use rust_i18n::t;

use crate::{git, workspace::ui_code_font};

use super::{
    CommitRow, GIT_BADGE_SIZE, GIT_RESIZE_GUTTER, GIT_TEXT_SIZE, MESSAGE_LINE_HEIGHT, Palette,
    file_count, icon, quiet_button, text_base_size,
};

use gpui_component::popover::Popover;

pub(super) const LEFT_INSET: Pixels = px(8.);
pub(super) const GRAPH_GAP: Pixels = px(4.);
pub(super) fn right_inset() -> Pixels {
    GIT_RESIZE_GUTTER + px(14.)
}

const BADGE_HEIGHT: Pixels = px(16.);
const BADGE_PADDING: Pixels = px(5.);
const BADGE_GAP: Pixels = px(4.);
const REF_ICON: Pixels = px(10.);
const REF_ICON_GAP: Pixels = px(3.);

pub(super) type References = Arc<HashMap<String, Vec<git::Reference>>>;

fn title_text(title_y: Rc<Cell<Pixels>>, cx: &App) -> Div {
    let line_height = px(text_base_size(cx) * MESSAGE_LINE_HEIGHT);
    div()
        .min_w_0()
        .text_size(px(text_base_size(cx)))
        .line_height(relative(MESSAGE_LINE_HEIGHT))
        .font_weight(FontWeight::NORMAL)
        .line_clamp(2)
        .text_ellipsis()
        .on_prepaint(move |bounds, window, _| {
            title_y.set(bounds.top() + window.pixel_snap(line_height) / 2.);
        })
}

fn badge() -> Div {
    h_flex()
        .h(BADGE_HEIGHT)
        .flex_shrink_0()
        .min_w_0()
        .px(BADGE_PADDING)
        .gap(REF_ICON_GAP)
        .rounded(px(4.))
        .text_size(GIT_BADGE_SIZE)
        .line_height(BADGE_HEIGHT)
}

fn ref_icon(reference: &git::Reference) -> IconName {
    if reference.tag {
        IconName::Tag
    } else {
        IconName::GitBranch
    }
}

fn text_width(text: &str, mut run: TextRun, size: Pixels, window: &Window) -> Pixels {
    run.len = text.len();
    window
        .text_system()
        .layout_line(text, size, &[run], None)
        .width
        .ceil()
}

pub(super) fn worktree_content(
    title: SharedString,
    count: Option<(usize, (usize, usize, usize))>,
    colors: Palette,
    main: Hsla,
    cx: &App,
    title_y: Rc<Cell<Pixels>>,
) -> impl IntoElement {
    v_flex()
        .id("git-worktree-label")
        .flex_1()
        .min_w_0()
        .gap(px(3.))
        .child(
            h_flex()
                .min_w_0()
                .gap(BADGE_GAP)
                .items_start()
                .child(title_text(title_y, cx).flex_1().child(title))
                .when_some(count, |line, (total, _)| {
                    line.child(
                        badge()
                            .bg(colors.soft)
                            .text_color(colors.muted)
                            .child(file_count(total)),
                    )
                }),
        )
        .when_some(
            count.filter(|(total, _)| *total != 0),
            |body, (_, (staged, unstaged, untracked))| {
                body.child(
                    h_flex()
                        .gap_2()
                        .overflow_hidden()
                        .font_family(ui_code_font(cx))
                        .text_size(GIT_TEXT_SIZE)
                        .line_height(px(15.))
                        .child(div().text_color(main).child(format!("S {staged}")))
                        .child(
                            div()
                                .text_color(colors.warning)
                                .child(format!("W {unstaged}")),
                        )
                        .child(
                            div()
                                .text_color(colors.accent)
                                .child(format!("? {untracked}")),
                        ),
                )
            },
        )
}

pub(super) fn commit_content(
    commit: &CommitRow,
    refs: &References,
    width: Pixels,
    lane_color: Hsla,
    colors: Palette,
    window: &Window,
    cx: &App,
    title_y: Rc<Cell<Pixels>>,
) -> impl IntoElement {
    let subject = if commit.subject.is_empty() {
        t!("git.empty_subject").into()
    } else {
        commit.subject.clone()
    };
    let mut regular = window.text_style().to_run(0);
    regular.font.weight = FontWeight::NORMAL;
    let mono_family = ui_code_font(cx);
    let mut mono = regular.clone();
    mono.font.family = mono_family.clone();
    let gap = px(6.);
    let hash_width = text_width(&commit.short, mono, GIT_TEXT_SIZE, window);
    let date_width = text_width(&commit.date, regular.clone(), GIT_TEXT_SIZE, window);
    let show_date = !commit.date.is_empty() && hash_width + gap + date_width <= width;
    let author_width = width - hash_width - gap - if show_date { date_width + gap } else { px(0.) };
    let title = title_text(title_y, cx).w_full().child(subject);
    let info = v_flex().w_full().min_w_0().gap(px(3.)).child(title).child(
        h_flex()
            .w_full()
            .min_w_0()
            .gap(gap)
            .overflow_hidden()
            .font_weight(FontWeight::NORMAL)
            .text_size(GIT_TEXT_SIZE)
            .line_height(px(15.))
            .text_color(colors.muted)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(mono_family)
                    .child(commit.short.clone()),
            )
            .when(
                author_width >= px(12.) && !commit.author.is_empty(),
                |row| {
                    row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(commit.author.clone()),
                    )
                },
            )
            .when(show_date, |row| {
                row.child(div().ml_auto().flex_shrink_0().child(commit.date.clone()))
            }),
    );
    v_flex().w_full().min_w_0().gap(px(3.)).child(info).when(
        commit.merge || refs.contains_key(commit.oid.as_str()),
        |body| {
            body.child(ref_strip(
                commit, refs, width, lane_color, colors, regular, window,
            ))
        },
    )
}

fn ref_strip(
    commit: &CommitRow,
    refs: &References,
    width: Pixels,
    lane_color: Hsla,
    colors: Palette,
    run: TextRun,
    window: &Window,
) -> impl IntoElement {
    let references = refs
        .get(commit.oid.as_str())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let merge = t!("git.merge");
    let measure_badge = |label: &str| {
        text_width(label, run.clone(), GIT_BADGE_SIZE, window) + BADGE_PADDING * 2. + px(2.)
    };
    let more_width = |count: usize| measure_badge(&format!("+{count}"));
    let budget = if commit.merge {
        (width - measure_badge(&merge) - BADGE_GAP).max(px(0.))
    } else {
        width
    };
    let mut visible = Vec::new();
    let mut used = px(0.);
    for reference in references {
        let width = measure_badge(&reference.label) + REF_ICON + REF_ICON_GAP;
        let next = used
            + width
            + if visible.is_empty() {
                px(0.)
            } else {
                BADGE_GAP
            };
        if next > budget {
            break;
        }
        visible.push(width);
        used = next;
    }
    // Give the first ref (HEAD when present) a chance to stay visible on narrow panes.
    while visible.len() < references.len() && !visible.is_empty() {
        let overflow = more_width(references.len() - visible.len());
        if used + BADGE_GAP + overflow <= budget {
            break;
        }
        used = (used - visible.pop().unwrap() - BADGE_GAP).max(px(0.));
    }
    if visible.is_empty() && !references.is_empty() {
        let available = if references.len() == 1 {
            budget
        } else {
            budget - more_width(references.len() - 1) - BADGE_GAP
        };
        if available >= px(32.) {
            visible.push(available);
        }
    }

    let shown = visible.len();
    let hidden = references.len() - shown;
    h_flex()
        .id("git-commit-refs")
        .h(BADGE_HEIGHT)
        .min_w_0()
        .gap(BADGE_GAP)
        .font_weight(FontWeight::NORMAL)
        .when(commit.merge, |row| {
            row.child(
                badge()
                    .bg(colors.soft)
                    .text_color(colors.muted)
                    .child(merge.into_owned()),
            )
        })
        .children(references.iter().zip(visible).map(|(reference, width)| {
            let label: SharedString = reference.label.clone().into();
            let color = if reference.tag {
                colors.ink
            } else {
                colors.ink.blend(lane_color.opacity(0.68))
            };
            badge()
                .w(width)
                .border_1()
                .border_color(if reference.head {
                    transparent_black()
                } else {
                    colors.border
                })
                .bg(if reference.head {
                    lane_color.opacity(0.16)
                } else if reference.tag {
                    colors.elevated
                } else {
                    lane_color.opacity(0.08)
                })
                .text_color(color)
                .child(icon(ref_icon(reference), color, REF_ICON.as_f32()))
                .child(div().min_w_0().truncate().child(label))
        }))
        .when(hidden > 0, |row| {
            let refs = refs.clone();
            let oid = commit.oid.clone();
            let label = format!("+{hidden}");
            row.child(
                Popover::new("git-ref-overflow")
                    .dropdown_motion()
                    .offset(px(4.))
                    .p(px(6.))
                    .w(px(300.))
                    .scrollable(false)
                    .trigger(
                        quiet_button("git-ref-overflow-trigger")
                            .h(BADGE_HEIGHT)
                            .w(more_width(hidden).min(budget))
                            .p_0()
                            .rounded(px(4.))
                            .label_size(GIT_BADGE_SIZE)
                            .border_1()
                            .border_color(colors.border)
                            .label(label)
                            .accessibility_label(t!("git.more_refs", count = hidden))
                            .on_click(|_, _, cx| cx.stop_propagation()),
                    )
                    .content(move |_, _, _| {
                        v_flex()
                            .id("git-hidden-refs")
                            .max_h(px(224.))
                            .overflow_y_scroll()
                            .gap(px(3.))
                            .text_size(GIT_TEXT_SIZE)
                            .children(
                                refs.get(oid.as_str())
                                    .into_iter()
                                    .flatten()
                                    .skip(shown)
                                    .map(|reference| {
                                        h_flex()
                                            .gap_2()
                                            .px_1()
                                            .py_0p5()
                                            .child(icon(
                                                ref_icon(reference),
                                                colors.muted,
                                                REF_ICON.as_f32(),
                                            ))
                                            .child(div().min_w_0().child(reference.label.clone()))
                                    }),
                            )
                    }),
            )
        })
}
