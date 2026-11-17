use rust_i18n::t;
use std::rc::Rc;

use gpui::{
    Action, App, AppContext as _, Context, ElementId, Entity, Focusable as _, Hsla,
    InteractiveElement as _, IntoElement, ParentElement, Render, ScrollAnchor, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px, relative,
};
use gpui_base::{TestSupportExt as _, input::CodeActionMenuState};
use gpui_kit_assets::IconName;

pub(crate) use gpui_base::input::CodeActionItem;

pub(super) const QUICK_FIX_BUTTON_SIZE: gpui::Pixels = px(22.);
const CODE_ACTION_ICON_SIZE: gpui::Pixels = px(14.);
const PREVIEW_BUTTON_INSET: gpui::Pixels = px(4.);

use crate::{
    ActiveTheme, Disableable as _, Icon, Selectable as _, Sizable,
    button::{Button, ButtonCustomVariant, ButtonVariants},
    h_flex,
    input::{self, EditorState},
    v_flex,
};

/// Renders actions inside the existing editor hover surface. It uses the
/// surface's scroll handle, including when navigating with the keyboard.
pub(crate) struct CodeActionMenu {
    editor: WeakEntity<EditorState>,
    state: CodeActionMenuState,
    items: Vec<Rc<CodeActionItem>>,
    anchors: Vec<ScrollAnchor>,
    selected: Option<usize>,
}

impl CodeActionMenu {
    pub(crate) fn new(editor: Entity<EditorState>, cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            editor: editor.downgrade(),
            state: CodeActionMenuState::default(),
            items: Vec::new(),
            anchors: Vec::new(),
            selected: None,
        })
    }

    pub(crate) fn sync(
        &mut self,
        state: CodeActionMenuState,
        scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) {
        if !state.open
            || self.items.len() != state.items.len()
            || self
                .items
                .iter()
                .zip(&state.items)
                .any(|(old, new)| old.provider_id != new.provider_id || old.action != new.action)
        {
            self.items = state.items.iter().cloned().map(Rc::new).collect();
            self.anchors = self
                .items
                .iter()
                .map(|_| ScrollAnchor::for_handle(scroll.clone()))
                .collect();
            self.selected = self
                .items
                .iter()
                .position(|item| item.action.disabled.is_none());
        }
        self.state = state;
        cx.notify();
    }

    fn choose(
        &self,
        item: &CodeActionItem,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.loading || item.action.disabled.is_some() {
            return;
        }
        let editor = self.editor.clone();
        let item = item.clone();
        window.defer(cx, move |window, cx| {
            let _ = editor.update(cx, |editor, cx| {
                if preview {
                    editor.preview_code_action(&item, window, cx);
                } else {
                    editor.perform_code_action(&item, window, cx);
                }
            });
        });
    }

    pub(crate) fn handle_action(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.state.open {
            return false;
        }
        if input::Enter::is_primary(&*action) {
            if let Some(item) = self.selected.and_then(|index| self.items.get(index)) {
                self.choose(item, false, window, cx);
            }
        } else if action.partial_eq(&input::Escape) {
            let editor = self.editor.clone();
            window.defer(cx, move |_, cx| {
                let _ = editor.update(cx, |editor, cx| editor.dismiss_code_action_overlay(cx));
            });
        } else if action.partial_eq(&input::MoveDown) || action.partial_eq(&input::MoveUp) {
            let enabled = |(_, item): &(usize, &Rc<CodeActionItem>)| item.action.disabled.is_none();
            let next = if action.partial_eq(&input::MoveDown) {
                self.items
                    .iter()
                    .enumerate()
                    .skip(self.selected.map_or(0, |index| index + 1))
                    .find(enabled)
            } else {
                self.items
                    .iter()
                    .enumerate()
                    .take(self.selected.unwrap_or(0))
                    .rev()
                    .find(enabled)
            };
            if let Some((index, _)) = next {
                self.selected = Some(index);
                self.anchors[index].scroll_to(window, cx);
                cx.notify();
            }
        } else {
            return false;
        }
        true
    }
}

impl Render for CodeActionMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = v_flex()
            .id("code-action-menu")
            .role(gpui::Role::Menu)
            .aria_label(t!("Input.Show Code Actions"))
            .test_support()
            .p(px(6.))
            .text_size(px(13.))
            .line_height(relative(1.35))
            .when(
                self.state
                    .hover
                    .as_ref()
                    .is_none_or(|hover| hover.diagnostics.is_empty()),
                |content| {
                    content.child(
                        h_flex()
                            .justify_between()
                            .px(px(6.))
                            .py(px(6.))
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(t!("Input.Code Actions")),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if cfg!(target_os = "macos") {
                                        "⌘ ."
                                    } else {
                                        "Ctrl ."
                                    }),
                            ),
                    )
                },
            );
        for (index, item) in self.items.iter().enumerate() {
            let reason = item
                .action
                .disabled
                .as_ref()
                .map(|disabled| disabled.reason.clone());
            let enabled = reason.is_none() && !self.state.loading;
            let preview = self
                .editor
                .upgrade()
                .is_some_and(|editor| editor.read(cx).code_action_can_preview(item));
            let selected = self.selected == Some(index);
            let is_previewed = self.state.preview.as_ref().is_some_and(|(action, _)| {
                action.provider_id == item.provider_id && action.action == item.action
            });
            let choice_icon_slot = px(16.);
            let (choice_icon, choice_icon_size) = if item.action.is_preferred == Some(true) {
                (IconName::Check, choice_icon_slot)
            } else {
                (IconName::Pencil, CODE_ACTION_ICON_SIZE)
            };
            let choice = h_flex()
                .id(("code-action-choice", index))
                .role(gpui::Role::MenuItem)
                .aria_label(item.action.title.clone())
                .test_support()
                .anchor_scroll(Some(self.anchors[index].clone()))
                .flex_1()
                .min_w_0()
                .items_start()
                .gap(px(8.))
                .px(px(7.))
                .py(px(6.))
                .when(enabled, |choice| {
                    choice.cursor_pointer().on_click(cx.listener({
                        let item = item.clone();
                        move |menu, _, window, cx| menu.choose(&item, false, window, cx)
                    }))
                })
                .when(reason.is_some(), |choice| {
                    choice.text_color(cx.theme().muted_foreground)
                })
                .child(
                    h_flex()
                        .flex_none()
                        .mt(px(1.))
                        .size(choice_icon_slot)
                        .justify_center()
                        .child(
                            Icon::new(choice_icon)
                                .size(choice_icon_size)
                                .text_color(cx.theme().muted_foreground),
                        ),
                )
                .child(div().min_w_0().child(item.action.title.clone()));
            let row = v_flex()
                .id(("code-action-row", index))
                .debug_selector(|| format!("code-action-row-{index}"))
                .rounded(px(6.))
                .pr(if preview {
                    PREVIEW_BUTTON_INSET
                } else {
                    px(2.)
                })
                .border_1()
                .border_color(cx.theme().transparent)
                .when(selected, |row| {
                    row.bg(cx.theme().accent.opacity(0.12))
                        .border_color(cx.theme().accent)
                })
                .when(enabled, |row| {
                    row.hover(|row| row.bg(cx.theme().list_hover))
                })
                .child(
                    h_flex()
                        .items_start()
                        .gap(px(2.))
                        .child(choice)
                        .when(preview, |row| {
                            row.child(
                                div()
                                    .flex_none()
                                    .my(PREVIEW_BUTTON_INSET)
                                    .debug_selector(|| {
                                        format!("preview-code-action-button-{index}")
                                    })
                                    .child(
                                        Button::new(("preview-code-action", index))
                                            .ghost()
                                            .with_size(QUICK_FIX_BUTTON_SIZE)
                                            .rounded(px(5.))
                                            .icon(IconName::Eye)
                                            .icon_size(CODE_ACTION_ICON_SIZE)
                                            .selected(is_previewed)
                                            .disabled(!enabled)
                                            .tab_stop(false)
                                            .tooltip(t!("Input.Preview Code Action"))
                                            .on_click(cx.listener({
                                                let item = item.clone();
                                                move |menu, _, window, cx| {
                                                    menu.choose(&item, true, window, cx)
                                                }
                                            })),
                                    ),
                            )
                        }),
                )
                .when_some(reason, |row, reason| {
                    row.child(
                        div()
                            .pl(px(32.))
                            .pr(px(8.))
                            .pb(px(6.))
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(reason),
                    )
                });
            content = content.child(row);
        }
        let status: Option<SharedString> = if self.state.loading {
            Some(t!("Input.Loading Code Actions").into())
        } else if let Some(error) = &self.state.error {
            Some(error.clone())
        } else if self.items.is_empty() {
            Some(t!("Input.No Code Actions").into())
        } else {
            None
        };
        content
            .when_some(status, |content, status| {
                content.child(
                    div()
                        .px(px(8.))
                        .py(px(6.))
                        .text_color(cx.theme().muted_foreground)
                        .child(status),
                )
            })
            .when_some(self.state.preview.as_ref(), |content, (_, preview)| {
                content.child(
                    div()
                        .id("code-action-preview")
                        .debug_selector(|| "code-action-preview".into())
                        .test_support()
                        .mx(px(8.))
                        .my(px(6.))
                        .pt(px(8.))
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .text_size(px(12.))
                        .line_height(relative(1.7))
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(preview.clone()),
                )
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(12.))
                    .px(px(8.))
                    .pt(px(7.))
                    .pb(px(2.))
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(Icon::new(IconName::ArrowUp).size(px(12.)))
                            .child(Icon::new(IconName::ArrowDown).size(px(12.)))
                            .child(t!("Input.Select Fix")),
                    )
                    .child(
                        h_flex()
                            .gap(px(4.))
                            .child(Icon::new(IconName::CornerDownLeft).size(px(12.)))
                            .child(t!("Input.Apply Fix")),
                    )
                    .child(t!("Input.Close Code Actions")),
            )
    }
}

pub(super) fn quick_fix_button(
    id: impl Into<ElementId>,
    label: SharedString,
    tone: Hsla,
    cx: &App,
) -> Button {
    Button::new(id)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(cx.theme().popover)
                .foreground(tone)
                .hover(
                    cx.theme()
                        .popover
                        .blend(cx.theme().foreground.opacity(0.07)),
                )
                .active(
                    cx.theme()
                        .popover
                        .blend(cx.theme().foreground.opacity(0.12)),
                ),
        )
        .with_size(QUICK_FIX_BUTTON_SIZE)
        .rounded(px(5.))
        .cursor_pointer()
        .border_1()
        .border_color(cx.theme().border)
        .icon(IconName::Lightbulb)
        .icon_size(CODE_ACTION_ICON_SIZE)
        .accessibility_label(label.clone())
        .tooltip(label)
        .tab_stop(false)
}

pub(crate) fn code_action_indicator(
    editor: &Entity<EditorState>,
    window: &mut Window,
    cx: &mut App,
) -> Option<gpui::AnyElement> {
    let state = editor.read(cx);
    if !state.focus_handle(cx).is_focused(window) {
        return None;
    }
    let bounds = state.code_action_indicator_bounds(QUICK_FIX_BUTTON_SIZE)?;
    let range = state.code_action_line_range();
    let severity = state.diagnostics().and_then(|diagnostics| {
        diagnostics
            .range(range.clone())
            .map(|entry| entry.severity)
            .min_by_key(|severity| match severity {
                gpui_base::input::DiagnosticSeverity::Error => 0,
                gpui_base::input::DiagnosticSeverity::Warning => 1,
                gpui_base::input::DiagnosticSeverity::Info => 2,
                gpui_base::input::DiagnosticSeverity::Hint => 3,
            })
    });
    let tone = severity
        .map(|severity| crate::highlighter::diagnostic_foreground(severity, cx))
        .unwrap_or(cx.theme().foreground);
    let editor = editor.downgrade();
    let button = div()
        .debug_selector(|| "code-action-lightbulb".into())
        .child(
            quick_fix_button(
                "code-action-lightbulb",
                t!("Input.Code Actions").into(),
                tone,
                cx,
            )
            .on_click(move |_, window, cx| {
                let _ = editor.update(cx, |editor, cx| {
                    if editor.code_action_menu_state().open {
                        editor.dismiss_code_action_overlay(cx);
                    } else {
                        editor.open_code_actions(range.clone(), window, cx);
                    }
                });
            }),
        );
    Some(
        gpui::deferred(gpui::anchored().position(bounds.origin).child(button))
            .with_priority(gpui_base::POPUP_PRIORITY - 1)
            .into_any_element(),
    )
}
