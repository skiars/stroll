use std::{ops::Range, rc::Rc};

use gpui::{
    Action, AnyElement, App, AppContext, Context, DismissEvent, Empty, Entity, EventEmitter,
    Focusable as _, FontWeight, HighlightStyle, InteractiveElement as _, IntoElement,
    ParentElement, Pixels, Render, RenderOnce, SharedString, Styled, StyledText, Subscription,
    WeakEntity, Window, deferred, div, prelude::FluentBuilder, px, relative,
};
use gpui_base::TestSupportExt as _;
use gpui_base::input::CompletionHighlighter;
use gpui_kit_assets::IconName;
use lsp_types::{CompletionItem, CompletionItemKind, CompletionItemTag, Documentation};

const MAX_MENU_HEIGHT: Pixels = px(240.);
const MIN_DOCUMENTATION_WIDTH: Pixels = px(200.);
const POPOVER_GAP: Pixels = px(6.);

/// Drop ranges a custom highlighter cannot safely paint, then order the rest.
///
/// A range past the label, or one that begins or ends inside a character,
/// would split a glyph. Empty ranges paint nothing, so they go too. What
/// remains is sorted and merged: GPUI builds text runs assuming highlights
/// are ordered and do not overlap, and an overlap makes the run longer than
/// the label.
fn retained_highlight_ranges(
    label: &str,
    ranges: impl IntoIterator<Item = Range<usize>>,
) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = ranges
        .into_iter()
        .filter(|range| {
            range.start < range.end
                && range.end <= label.len()
                && label.is_char_boundary(range.start)
                && label.is_char_boundary(range.end)
        })
        .collect();
    ranges.sort_by_key(|range| range.start);

    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

use crate::{
    ActiveTheme, Icon, IndexPath, Selectable, actions, h_flex,
    input::{
        self, EditorState,
        popovers::{CaretAnchoredPopup, editor_popover, render_markdown},
    },
    list::{List, ListDelegate, ListEvent, ListState},
    menu::MenuItemElement,
    scroll::ScrollableElement as _,
};

use super::caret_anchored::CARET_POPUP_MARGIN;

fn kind_icon(kind: Option<CompletionItemKind>) -> IconName {
    match kind {
        Some(
            CompletionItemKind::METHOD
            | CompletionItemKind::FUNCTION
            | CompletionItemKind::CONSTRUCTOR,
        ) => IconName::SquareFunction,
        Some(
            CompletionItemKind::CLASS | CompletionItemKind::INTERFACE | CompletionItemKind::STRUCT,
        ) => IconName::Box,
        Some(CompletionItemKind::ENUM | CompletionItemKind::ENUM_MEMBER) => IconName::List,
        Some(CompletionItemKind::FIELD | CompletionItemKind::PROPERTY) => IconName::Tags,
        Some(CompletionItemKind::VARIABLE | CompletionItemKind::VALUE) => IconName::Variable,
        Some(CompletionItemKind::CONSTANT) => IconName::Hash,
        Some(CompletionItemKind::KEYWORD | CompletionItemKind::OPERATOR) => IconName::Code,
        Some(CompletionItemKind::MODULE) => IconName::Package,
        Some(CompletionItemKind::FILE) => IconName::File,
        Some(CompletionItemKind::FOLDER) => IconName::Folder,
        Some(CompletionItemKind::TYPE_PARAMETER) => IconName::Type,
        Some(CompletionItemKind::SNIPPET) => IconName::Braces,
        Some(CompletionItemKind::EVENT) => IconName::Zap,
        _ => IconName::TextCursorInput,
    }
}

struct ContextMenuDelegate {
    query: SharedString,
    menu: Entity<CompletionMenu>,
    items: Vec<Rc<CompletionItem>>,
    selected_ix: usize,
    highlight: Rc<CompletionHighlighter>,
}

impl ContextMenuDelegate {
    fn set_items(&mut self, items: Vec<CompletionItem>, preserve_selection: bool) {
        self.selected_ix = self
            .selected_item()
            .filter(|_| preserve_selection)
            .and_then(|selected| {
                items.iter().position(|item| {
                    item.label == selected.label
                        && item.label_details == selected.label_details
                        && item.kind == selected.kind
                        && item.detail == selected.detail
                        && item.filter_text == selected.filter_text
                        && item.insert_text == selected.insert_text
                })
            })
            .unwrap_or(0);
        self.items = items.into_iter().map(Rc::new).collect();
    }

    fn selected_item(&self) -> Option<&Rc<CompletionItem>> {
        self.items.get(self.selected_ix)
    }
}

#[derive(IntoElement)]
struct CompletionMenuItem {
    ix: usize,
    item: Rc<CompletionItem>,
    children: Vec<AnyElement>,
    selected: bool,
    query: SharedString,
    highlight: Rc<CompletionHighlighter>,
}

impl CompletionMenuItem {
    fn new(
        ix: usize,
        item: Rc<CompletionItem>,
        query: SharedString,
        highlight: Rc<CompletionHighlighter>,
    ) -> Self {
        Self {
            ix,
            item,
            children: vec![],
            selected: false,
            query,
            highlight,
        }
    }
}
impl Selectable for CompletionMenuItem {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl ParentElement for CompletionMenuItem {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for CompletionMenuItem {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let item = self.item;
        let text_size = window.text_style().font_size.to_pixels(window.rem_size());

        let deprecated = item.deprecated.unwrap_or(false)
            || item
                .tags
                .as_ref()
                .is_some_and(|tags| tags.contains(&CompletionItemTag::DEPRECATED));
        let highlights = retained_highlight_ranges(
            &item.label,
            (self.highlight)(self.query.as_ref(), item.as_ref()),
        )
        .into_iter()
        .map(|range| {
            (
                range,
                HighlightStyle {
                    color: Some(cx.theme().link),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    ..Default::default()
                },
            )
        })
        .collect::<Vec<_>>();

        MenuItemElement::new(self.ix)
            .aria_label(item.label.clone())
            .selected(self.selected)
            .w_full()
            .text_size(text_size)
            .py_0()
            .px_2()
            .h(text_size + px(11.))
            .rounded(cx.theme().radius)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Icon::new(kind_icon(item.kind))
                            .size(text_size)
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(deprecated, |this| this.line_through())
                            .child(StyledText::new(item.label.clone()).with_highlights(highlights)),
                    )
                    .when_some(
                        item.detail
                            .as_deref()
                            .and_then(|detail| detail.lines().next())
                            .filter(|detail| !detail.is_empty()),
                        |this, detail| {
                            this.child(
                                div()
                                    .min_w_0()
                                    .max_w(relative(0.42))
                                    .truncate()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(SharedString::from(detail.to_owned())),
                            )
                        },
                    ),
            )
            .children(self.children)
    }
}

impl EventEmitter<DismissEvent> for ContextMenuDelegate {}

impl ListDelegate for ContextMenuDelegate {
    type Item = CompletionMenuItem;

    fn items_count(&self, _: usize, _: &gpui::App) -> usize {
        self.items.len()
    }

    fn render_item(
        &mut self,
        ix: crate::IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let item = self.items.get(ix.row)?;
        Some(CompletionMenuItem::new(
            ix.row,
            item.clone(),
            self.query.clone(),
            self.highlight.clone(),
        ))
    }

    fn set_selected_index(
        &mut self,
        ix: Option<crate::IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected_ix = ix.map(|i| i.row).unwrap_or(0);
        cx.notify();
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(item) = self.selected_item() else {
            return;
        };

        self.menu.update(cx, |this, cx| {
            this.select_item(&item, window, cx);
        });
    }
}

/// A context menu for code completions and code actions.
pub struct CompletionMenu {
    offset: usize,
    revision: Option<(u64, u64)>,
    editor: WeakEntity<EditorState>,
    list: Entity<ListState<ContextMenuDelegate>>,
    open: bool,

    /// The offset of the first character that triggered the completion.
    pub(crate) trigger_start_offset: Option<usize>,
    query: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl CompletionMenu {
    /// Creates a new `CompletionMenu` with the given offset and completion items.
    ///
    /// NOTE: This element should not call from EditorState::new, unless that will stack overflow.
    pub(crate) fn new(
        editor: Entity<EditorState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let view = cx.entity();
            let menu = ContextMenuDelegate {
                query: SharedString::default(),
                menu: view,
                items: vec![],
                selected_ix: 0,
                highlight: Rc::new(gpui_base::input::highlight_prefix),
            };

            let list = cx.new(|cx| ListState::new(menu, window, cx));

            let _subscriptions = vec![cx.subscribe(&list, |_: &mut Self, _, _: &ListEvent, cx| {
                cx.notify();
            })];

            Self {
                offset: 0,
                revision: None,
                editor: editor.downgrade(),
                list,
                open: false,
                trigger_start_offset: None,
                query: SharedString::default(),
                _subscriptions,
            }
        })
    }

    fn select_item(&mut self, item: &CompletionItem, window: &mut Window, cx: &mut Context<Self>) {
        let item = item.clone();
        let range = self.trigger_start_offset.unwrap_or(self.offset)..self.offset;

        let editor = self.editor.clone();
        let offset = self.offset;
        let revision = self.revision;

        cx.spawn_in(window, async move |_, cx| {
            editor.update_in(cx, |editor, window, cx| {
                let completion = editor.completion_menu_state();
                if editor.cursor() == offset
                    && editor.focus_handle(cx).is_focused(window)
                    && revision.is_none_or(|(document, menu)| {
                        completion.open
                            && editor.document_revision() == document
                            && completion.revision() == menu
                    })
                {
                    editor.dismiss_completion_overlay(cx);
                    editor.insert_completion(&item, range, window, cx);
                }
            })
        })
        .detach();
    }

    pub(crate) fn handle_action(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.open {
            return false;
        }

        cx.propagate();
        if input::Enter::is_primary(&*action) || action.partial_eq(&input::IndentInline) {
            let Some(item) = self.list.read(cx).delegate().selected_item().cloned() else {
                return false;
            };
            self.select_item(&item, window, cx);
        } else if action.partial_eq(&input::Escape) {
            self.on_action_escape(window, cx);
        } else if action.partial_eq(&input::MoveUp) {
            self.on_action_up(window, cx);
        } else if action.partial_eq(&input::MoveDown) {
            self.on_action_down(window, cx);
        } else {
            return false;
        }

        cx.stop_propagation();
        true
    }

    fn on_action_escape(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);
    }

    fn on_action_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |this, cx| {
            this.on_action_select_prev(&actions::SelectUp, window, cx)
        });
    }

    fn on_action_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |this, cx| {
            this.on_action_select_next(&actions::SelectDown, window, cx)
        });
    }

    /// Synchronize a hidden editor menu without dismissing a newer response.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.trigger_start_offset = None;
        cx.notify();
    }

    /// Dismiss the menu in response to a user action.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.hide(cx);
        let editor = self.editor.clone();
        let revision = self.revision;
        cx.spawn(async move |_, cx| {
            let _ = editor.update(cx, |editor, cx| {
                if revision.is_none_or(|(_, revision)| {
                    editor.completion_menu_state().revision() == revision
                }) {
                    editor.dismiss_completion_overlay(cx);
                }
            });
        })
        .detach();
    }

    pub(crate) fn set_revision(&mut self, document: u64, menu: u64) {
        self.revision = Some((document, menu));
    }

    pub(crate) fn update_query(&mut self, start_offset: usize, query: impl Into<SharedString>) {
        self.trigger_start_offset = Some(start_offset);
        self.query = query.into();
    }

    pub(crate) fn show(
        &mut self,
        offset: usize,
        items: impl Into<Vec<CompletionItem>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = items.into();
        let preserve_selection = self.open && offset != self.offset;
        self.offset = offset;
        self.open = true;
        self.list.update(cx, |this, cx| {
            let delegate = this.delegate_mut();
            delegate.query = self.query.clone();
            delegate.set_items(items, preserve_selection);
            let selected = delegate.selected_ix;
            this.set_selected_index(Some(IndexPath::new(selected)), window, cx);
        });

        cx.notify();
    }

    /// Replace the label highlighter and repaint an open menu.
    pub(crate) fn set_highlight(
        &mut self,
        highlight: Rc<CompletionHighlighter>,
        cx: &mut Context<Self>,
    ) {
        self.list.update(cx, |list, cx| {
            if Rc::ptr_eq(&list.delegate().highlight, &highlight) {
                return;
            }
            list.delegate_mut().highlight = highlight;
            cx.notify();
        });
    }
}

impl Render for CompletionMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return Empty.into_any_element();
        }

        if self.list.read(cx).delegate().items.is_empty() {
            self.open = false;
            return Empty.into_any_element();
        }

        let Some(editor) = self.editor.upgrade() else {
            return Empty.into_any_element();
        };
        // The anchor is resolved in the popup's deferred prepaint from the
        // input's current-frame geometry; the content below is built there.
        let configured_max = editor.read(cx).lsp().completion_menu.max_width;
        let list = self.list.clone();
        let menu = cx.weak_entity();

        deferred(CaretAnchoredPopup::new(
            self.editor.clone(),
            move |anchor, available_height, code_font_size, window, cx| {
                let viewport_width = window.viewport_size().width;
                let menu_width =
                    configured_max.min((viewport_width - CARET_POPUP_MARGIN * 2.).max(px(0.)));
                let width_to_right = viewport_width
                    - CARET_POPUP_MARGIN
                    - anchor.x.max(CARET_POPUP_MARGIN)
                    - menu_width
                    - POPOVER_GAP;
                let selected_documentation = list
                    .read(cx)
                    .delegate()
                    .selected_item()
                    .and_then(|item| item.documentation.as_ref())
                    .map(|documentation| match documentation {
                        Documentation::String(text) => text,
                        Documentation::MarkupContent(markup) => &markup.value,
                    })
                    .filter(|text| !text.trim().is_empty())
                    .cloned();
                let stacked =
                    selected_documentation.is_some() && width_to_right < MIN_DOCUMENTATION_WIDTH;
                let documentation_width = if stacked {
                    menu_width
                } else {
                    width_to_right.min(configured_max)
                };
                let panel_insets = window.rem_size() * 0.5 + px(2.);
                let viewport_height = (available_height - panel_insets).max(px(0.));
                let (list_height, documentation_height) = if stacked {
                    let content_height = (viewport_height - panel_insets - POPOVER_GAP).max(px(0.));
                    let list_height = MAX_MENU_HEIGHT.min(content_height * 0.55);
                    (list_height, content_height - list_height)
                } else {
                    (MAX_MENU_HEIGHT.min(viewport_height), viewport_height)
                };
                let code_font = window.text_style().font().family;

                div()
                    .flex()
                    .flex_row()
                    .gap(POPOVER_GAP)
                    .items_start()
                    .when(stacked, |this| this.flex_col())
                    .child(
                        editor_popover("completion-menu", cx)
                            .debug_selector(|| "completion-menu".into())
                            .test_support()
                            .font_family(code_font)
                            .text_size((code_font_size - px(0.5)).max(px(1.)))
                            .w(menu_width)
                            .child(List::new(&list).popup_scrollbar().max_h(list_height)),
                    )
                    .when_some(selected_documentation, |this, doc| {
                        this.child(
                            editor_popover("completion-documentation", cx)
                                .debug_selector(|| "completion-documentation".into())
                                .test_support()
                                .w(documentation_width)
                                .child(
                                    div()
                                        .max_h(documentation_height)
                                        .px_2()
                                        .py_0p5()
                                        .child(render_markdown("doc", doc, window, cx))
                                        .overflow_y_popup_scrollbar(),
                                ),
                        )
                    })
                    .on_mouse_down_out({
                        let menu = menu.clone();
                        move |_, _, cx: &mut App| {
                            if let Some(menu) = menu.upgrade() {
                                menu.update(cx, |menu, cx| menu.dismiss(cx));
                            }
                        }
                    })
                    .into_any_element()
            },
        ))
        .with_priority(gpui_base::POPUP_PRIORITY)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_ranges_are_ordered_and_do_not_overlap() {
        assert_eq!(
            retained_highlight_ranges("flag", [2..3, 0..1]),
            vec![0..1, 2..3]
        );
        assert_eq!(retained_highlight_ranges("flag", [0..3, 2..4]), vec![0..4]);
        assert_eq!(
            retained_highlight_ranges("flag", [0..1, 1..2, 5..9, 2..3]),
            vec![0..3]
        );
        assert_eq!(retained_highlight_ranges("你", [0..1, 0..3]), vec![0..3]);
    }

    #[gpui::test]
    fn hiding_an_old_menu_does_not_dismiss_a_new_completion_response(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(crate::init);
        let window = cx.add_empty_window();
        let (editor, menu) = window.update(|window, cx| {
            let editor = cx.new(|cx| EditorState::new(window, cx).default_value("co"));
            let menu = CompletionMenu::new(editor.clone(), window, cx);
            let old_items = vec![CompletionItem {
                label: "const".into(),
                ..Default::default()
            }];
            editor.update(cx, |editor, cx| {
                editor.present_completion_items(0, "c", old_items.clone(), cx);
            });
            menu.update(cx, |menu, cx| {
                menu.update_query(0, "c");
                menu.show(1, old_items, window, cx);
                // The overlay observes the old request being hidden while a
                // replacement request is pending. Its deferred dismissal must
                // not act on the replacement response that arrives next.
                menu.hide(cx);
            });
            editor.update(cx, |editor, cx| {
                editor.present_completion_items(
                    0,
                    "co",
                    vec![CompletionItem {
                        label: "const".into(),
                        ..Default::default()
                    }],
                    cx,
                );
            });
            (editor, menu)
        });

        cx.run_until_parked();

        cx.read(|cx| {
            let completion = editor.read(cx).completion_menu_state();
            assert!(completion.open, "an old menu dismissed the new response");
            assert_eq!(completion.query, "co");
            assert_eq!(completion.items.len(), 1);
            assert_eq!(completion.items[0].label, "const");
        });
        drop(menu);
    }
}
