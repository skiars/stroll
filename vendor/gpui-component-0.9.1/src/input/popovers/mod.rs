mod caret_anchored;
mod code_action_menu;
mod completion_menu;
mod hover_popover;

pub(crate) use caret_anchored::*;
pub(crate) use code_action_menu::*;
pub(crate) use completion_menu::*;
pub(crate) use hover_popover::*;

use gpui::{
    App, Div, ElementId, InteractiveElement as _, IntoElement, SharedString, Stateful,
    StyleRefinement, Styled as _, Window, div, px, relative, rems,
};

use crate::{
    ActiveTheme, ThemeStyled as _,
    text::{TextView, TextViewStyle},
};

pub(super) fn render_markdown(
    id: impl Into<ElementId>,
    markdown: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> TextView {
    // Every caller renders its popover inside an editor, so the text style
    // active here is the one the editor was drawn with: code takes that
    // family, the prose keeps the UI font.
    let code_font = window.text_style().font().family;
    TextView::markdown(id, markdown)
        .style(
            TextViewStyle::default()
                .paragraph_gap(rems(0.35))
                .rule_spacing(px(6.))
                .heading_top_gap(px(8.))
                .heading_bottom_gap(px(0.))
                .heading_font_size(|level, rem_size| match level {
                    1..=3 => rem_size * 1,
                    4 => rem_size * 0.9,
                    _ => rem_size * 0.8,
                })
                .code_block(
                    StyleRefinement::default()
                        .bg(cx.theme().transparent)
                        .border_0()
                        .p_0()
                        .text_size(px(12.5))
                        .line_height(relative(1.5))
                        .font_family(code_font.clone()),
                ),
        )
        .font_family(cx.theme().font_family.clone())
        .text_size(px(12.5))
        .line_height(relative(1.6))
        .inline_code_style(
            StyleRefinement::default()
                .bg(if cx.theme().is_dark() {
                    gpui::rgb(0x292b33)
                } else {
                    gpui::rgb(0xf4f5f7)
                })
                .border_0()
                .rounded(px(3.))
                .px(px(3.))
                .font_family(code_font),
        )
        .code_block_actions(|_, _, _| div().into_any_element())
        .selectable(true)
}

pub(super) fn editor_popover(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .occlude()
        .popover_style(cx)
        .text_xs()
        .p_1()
}
