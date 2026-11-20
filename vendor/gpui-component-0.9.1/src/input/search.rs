use gpui_base::input::{InputModeKind, SearchSession};
use ropey::Rope;
use rust_i18n::t;
use std::ops::Range;

use gpui::{
    App, AppContext as _, Context, Empty, Entity, FocusHandle, Focusable, Half,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Pixels, Render, Styled,
    Subscription, WeakEntity, Window, actions, div, prelude::FluentBuilder as _, px,
};

use crate::{
    ActiveTheme, Disableable, ElementExt, IconName, Selectable, Sizable,
    button::{Button, ButtonVariants},
    h_flex,
    input::{
        Enter, Escape, IndentInline, Input, InputBaseState, InputEvent, InputState, OutdentInline,
        Replace,
    },
    label::Label,
    text::{RangeHighlight, TextViewState},
    v_flex,
};

const CONTEXT: &'static str = "SearchPanel";
const DOCUMENT_HIGHLIGHT_LIMIT: usize = 5000;

actions!(input, [Tab]);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Escape, Some("DocumentFind"))]);
}

pub type DocumentFind = SearchPanel<TextViewState>;

/// A surface that supplies text and presents the shared panel's matches.
#[doc(hidden)]
pub trait SearchTarget: Sized + 'static {
    fn update_query(&mut self, query: &str, session: &mut SearchSession, cx: &mut Context<Self>);
    fn step_match(&mut self, previous: bool, session: &mut SearchSession, cx: &mut Context<Self>);
    fn close_search(&mut self, cx: &mut Context<Self>);
    fn focus_search_target(&mut self, window: &mut Window, cx: &mut Context<Self>);
    fn can_replace(&self) -> bool {
        false
    }
    fn set_replace_mode(&mut self, _: bool, _: &mut Context<Self>) {}
    fn replace_match(&mut self, _: &str, _: bool, _: &mut Window, _: &mut Context<Self>) {}
}

impl<M: InputModeKind> SearchTarget for InputBaseState<M> {
    fn update_query(&mut self, query: &str, session: &mut SearchSession, cx: &mut Context<Self>) {
        self.set_search_query(query, session.case_insensitive, cx);
        *session = self.search_session().clone();
    }

    fn step_match(&mut self, previous: bool, session: &mut SearchSession, cx: &mut Context<Self>) {
        if previous {
            self.previous_search_match(cx);
        } else {
            self.next_search_match(cx);
        }
        *session = self.search_session().clone();
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        InputBaseState::close_search(self, cx);
    }

    fn focus_search_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus(window, cx);
    }

    fn can_replace(&self) -> bool {
        self.is_replaceable()
    }

    fn set_replace_mode(&mut self, replace: bool, cx: &mut Context<Self>) {
        self.set_search_replace_mode(replace, cx);
    }

    fn replace_match(
        &mut self,
        replacement: &str,
        all: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if all {
            self.replace_all_search_matches(replacement, window, cx);
        } else {
            self.replace_current_search_match(replacement, window, cx);
        }
    }
}

impl SearchTarget for TextViewState {
    fn update_query(&mut self, query: &str, session: &mut SearchSession, cx: &mut Context<Self>) {
        session.matcher.update_document(
            Rope::from(self.rendered_text().as_str()),
            query,
            session.case_insensitive,
        );
        paint_document_matches(self, session, cx);
    }

    fn step_match(&mut self, previous: bool, session: &mut SearchSession, cx: &mut Context<Self>) {
        if previous {
            session.matcher.next_back();
        } else {
            session.matcher.next();
        }
        paint_document_matches(self, session, cx);
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        self.clear_range_highlights(cx);
    }

    fn focus_search_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle().clone().focus(window, cx);
    }
}

fn paint_document_matches(
    text: &mut TextViewState,
    session: &SearchSession,
    cx: &mut Context<TextViewState>,
) {
    let accent = cx.theme().accent;
    let ranges = session.matcher.matched_ranges();
    let active = session.matcher.current().map(|index| &ranges[index]);
    // Bound the passive highlights while keeping the entire match list navigable.
    let passive = ranges
        .iter()
        .take(DOCUMENT_HIGHLIGHT_LIMIT)
        .filter(|range| Some(*range) != active)
        .map(|range| RangeHighlight::new(range.clone(), accent.opacity(0.22)));
    let active_highlight =
        active.map(|range| RangeHighlight::new(range.clone(), accent.opacity(0.48)));
    _ = text.set_range_highlights(passive.chain(active_highlight), cx);
    if let Some(range) = active {
        _ = text.scroll_to_range(range.clone(), None, true, cx);
    }
}

#[cfg(test)]
use gpui_base::input::SearchMatcher;

#[derive(Clone, Copy)]
#[cfg(test)]
enum MoveDirection {
    Up,
    Down,
}

#[cfg(test)]
fn next_scroll_direction(
    previous_match_ix: usize,
    current_match_ix: usize,
) -> Option<MoveDirection> {
    if current_match_ix <= previous_match_ix {
        None
    } else {
        Some(MoveDirection::Down)
    }
}

#[cfg(test)]
fn prev_scroll_direction(
    previous_match_ix: usize,
    current_match_ix: usize,
) -> Option<MoveDirection> {
    if current_match_ix >= previous_match_ix {
        None
    } else {
        Some(MoveDirection::Up)
    }
}

#[doc(hidden)]
pub struct SearchPanel<T: SearchTarget> {
    target: WeakEntity<T>,
    search_input: Entity<InputState>,
    replace_input: Entity<InputState>,
    session: gpui_base::input::SearchSession,
    input_width: Pixels,

    _subscriptions: Vec<Subscription>,
}

impl<T: SearchTarget> SearchPanel<T> {
    pub(super) fn replace_mode(&self) -> bool {
        self.session.replace_mode
    }

    pub(super) fn sync_session(&mut self, session: &gpui_base::input::SearchSession) {
        self.session = session.clone();
    }

    /// The query the panel's search input currently holds.
    pub(super) fn query(&self, cx: &App) -> gpui::SharedString {
        self.search_input.read(cx).value()
    }

    /// The panel's search input, so a test can assert what re-invoking search
    /// does to its value and selection.
    #[cfg(test)]
    pub(super) fn search_input(&self) -> &Entity<InputState> {
        &self.search_input
    }

    pub(crate) fn new(target: Entity<T>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let search_input = cx.new(|cx| InputState::new(window, cx));
        let replace_input = cx.new(|cx| InputState::new(window, cx));

        cx.new(|cx| {
            let _subscriptions =
                vec![
                    cx.subscribe(&search_input, |this: &mut Self, _, ev: &InputEvent, cx| {
                        // Handle search input changes
                        match ev {
                            InputEvent::Change => {
                                this.update_search_query(None, cx);
                            }
                            _ => {}
                        }
                    }),
                ];

            Self {
                target: target.downgrade(),
                search_input,
                replace_input,
                session: gpui_base::input::SearchSession::default(),
                input_width: Pixels::ZERO,
                _subscriptions,
            }
        })
    }

    pub(super) fn show_with_focus(
        &mut self,
        selected_text: &Rope,
        replace_mode: bool,
        visible_range_offset: Option<Range<usize>>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session.open = true;
        self.session.replace_mode = replace_mode;
        if focus {
            self.search_input
                .update(cx, |input, cx| input.focus(window, cx));
        }

        self.search_input.update(cx, |this, cx| {
            if selected_text.len() > 0 {
                this.set_value(selected_text.to_string(), window, cx);
            }
            this.select_all(window, cx);
        });

        // The `set_value` does not emit `InputEvent::Change`, so update the query
        // here to match the value of the search input.
        self.update_search_query(visible_range_offset, cx);
    }

    /// Update the matcher by the value of the search input.
    ///
    /// The `visible_range_offset` is to select the nearest match of the visible range,
    /// it is passed in, because the editor may be borrowed by the caller.
    fn update_search_query(
        &mut self,
        visible_range_offset: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        let query = self.search_input.read(cx).value();
        _ = self.target.update(cx, |target, cx| {
            target.update_query(&query, &mut self.session, cx);
        });
        if let Some(visible_range_offset) = visible_range_offset {
            self.session
                .matcher
                .update_cursor_by_offset(visible_range_offset.start);
        }
        cx.notify();
    }

    fn replaceable(&self, cx: &App) -> bool {
        self.target
            .read_with(cx, |target, _| target.can_replace())
            .unwrap_or(false)
    }

    pub(super) fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hide_with_focus(true, window, cx);
    }

    pub(super) fn hide_with_focus(
        &mut self,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.session.open = false;
        _ = self.target.update(cx, |target, cx| {
            target.close_search(cx);
            if focus_editor {
                target.focus_search_target(window, cx);
            }
        });
        cx.notify();
    }

    fn on_action_enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if action.shift {
            self.step(true, cx);
        } else if !action.secondary
            && self.session.replace_mode
            && self
                .replace_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        {
            self.replace_next(window, cx);
        } else {
            self.step(false, cx);
        }
    }

    fn on_action_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        self.hide(window, cx);
    }

    fn on_action_tab(&mut self, _: &IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_focus(window, cx);
    }

    fn on_action_tab_prev(
        &mut self,
        _: &OutdentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_focus(window, cx);
    }

    /// Cycle focus between the search and the replace input, to keep the Tab key
    /// staying in the panel.
    ///
    /// There are only 2 inputs, so the forward and the backward are the same.
    fn cycle_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.session.replace_mode || !self.replaceable(cx) {
            return;
        }

        let search_focus_handle = self.search_input.read(cx).focus_handle(cx);
        let focus_handle = if search_focus_handle.is_focused(window) {
            self.replace_input.read(cx).focus_handle(cx)
        } else {
            search_focus_handle
        };
        focus_handle.focus(window, cx);
    }

    fn on_action_replace(&mut self, _: &Replace, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_replace_mode(window, cx);
    }

    /// Toggle the replace field, and move focus to the field that is going to be used.
    fn toggle_replace_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            return;
        }

        self.session.replace_mode = !self.session.replace_mode;
        let replace_mode = self.session.replace_mode;
        _ = self
            .target
            .update(cx, |target, cx| target.set_replace_mode(replace_mode, cx));
        let focus_handle = if self.session.replace_mode {
            self.replace_input.read(cx).focus_handle(cx)
        } else {
            self.search_input.read(cx).focus_handle(cx)
        };
        focus_handle.focus(window, cx);
        cx.notify();
    }

    fn step(&mut self, previous: bool, cx: &mut Context<Self>) {
        _ = self.target.update(cx, |target, cx| {
            target.step_match(previous, &mut self.session, cx);
        });
        cx.notify();
    }

    fn replace_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            self.session.replace_mode = false;
            cx.notify();
            return;
        }

        let replacement = self.replace_input.read(cx).value();
        _ = self.target.update(cx, |target, cx| {
            target.replace_match(&replacement, false, window, cx)
        });
    }

    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replaceable(cx) {
            self.session.replace_mode = false;
            cx.notify();
            return;
        }

        let replacement = self.replace_input.read(cx).value();
        _ = self.target.update(cx, |target, cx| {
            target.replace_match(&replacement, true, window, cx)
        });
    }
}

impl DocumentFind {
    pub fn for_document(
        target: Entity<TextViewState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        Self::new(target, window, cx)
    }

    pub fn is_open(&self) -> bool {
        self.session.open
    }

    pub fn set_placeholder(
        &mut self,
        label: impl Into<gpui::SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_input
            .update(cx, |input, cx| input.set_placeholder(label, window, cx));
    }

    pub fn has_matches(&self) -> bool {
        self.session.open && !self.session.matcher.is_empty()
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_open() {
            self.dismiss(window, cx);
        } else {
            self.open_with_query("", window, cx);
        }
    }

    /// Show Find, seeding the field when a nonempty query is supplied.
    pub fn open_with_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.show_with_focus(&Rope::from(query), false, None, true, window, cx);
    }

    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_open() {
            self.hide(window, cx);
        } else {
            cx.propagate();
        }
    }
}

impl<T: SearchTarget> SearchPanel<T> {
    pub(super) fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.search_input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
            || self
                .replace_input
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
    }
}

impl<T: SearchTarget> Focusable for SearchPanel<T> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_input.read(cx).focus_handle(cx)
    }
}

impl<T: SearchTarget> Render for SearchPanel<T> {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.session.open {
            return Empty.into_any_element();
        }

        let has_matches = !self.session.matcher.is_empty();
        let allow_replace = self.replaceable(cx);
        if !allow_replace {
            self.session.replace_mode = false;
        }

        v_flex()
            .id("search-panel")
            .occlude()
            .track_focus(&self.focus_handle(cx))
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_action_enter))
            .on_action(cx.listener(Self::on_action_escape))
            .on_action(cx.listener(Self::on_action_tab))
            .on_action(cx.listener(Self::on_action_tab_prev))
            .on_action(cx.listener(Self::on_action_replace))
            .font_family(cx.theme().font_family.clone())
            .items_center()
            .py_2()
            .px_3()
            .w_full()
            .gap_1()
            .bg(cx.theme().tokens.popover)
            .border_b_1()
            .rounded(cx.theme().radius.half())
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .gap_1()
                            .child(
                                Input::new(&self.search_input)
                                    .aria_label(t!("ComboBox.search_placeholder"))
                                    .focus_bordered(false)
                                    .suffix(
                                        Button::new("case-insensitive")
                                            .rounded(px(5.))
                                            .selected(!self.session.case_insensitive)
                                            .toggled(!self.session.case_insensitive)
                                            .xsmall()
                                            .compact()
                                            .ghost()
                                            .icon(IconName::CaseSensitive)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.session.case_insensitive =
                                                    !this.session.case_insensitive;
                                                this.update_search_query(None, cx);
                                                cx.notify();
                                            })),
                                    )
                                    .w_full()
                                    .shadow_none(),
                            )
                            .on_prepaint({
                                let view = cx.entity();
                                move |bounds, _, cx| {
                                    view.update(cx, |r, _| r.input_width = bounds.size.width)
                                }
                            }),
                    )
                    .when(allow_replace, |this| {
                        this.child(
                            Button::new("replace-mode")
                                .rounded(px(5.))
                                .xsmall()
                                .ghost()
                                .icon(IconName::Replace)
                                .selected(self.session.replace_mode)
                                .toggled(self.session.replace_mode)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_replace_mode(window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("prev")
                            .rounded(px(5.))
                            .xsmall()
                            .ghost()
                            .icon(IconName::ChevronLeft)
                            .disabled(!has_matches)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step(true, cx);
                            })),
                    )
                    .child(
                        Button::new("next")
                            .rounded(px(5.))
                            .xsmall()
                            .ghost()
                            .icon(IconName::ChevronRight)
                            .disabled(!has_matches)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step(false, cx);
                            })),
                    )
                    .child(
                        Label::new(self.session.matcher.label())
                            .when(!has_matches, |this| {
                                this.text_color(cx.theme().muted_foreground)
                            })
                            .text_left()
                            .min_w_16(),
                    )
                    .child(div().w_7())
                    .child(
                        Button::new("close")
                            .rounded(px(5.))
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_action_escape(&Escape, window, cx);
                            })),
                    ),
            )
            .when(self.session.replace_mode && allow_replace, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            Input::new(&self.replace_input)
                                .focus_bordered(false)
                                .w(self.input_width)
                                .shadow_none(),
                        )
                        .child(
                            Button::new("replace-one")
                                .rounded(px(5.))
                                .small()
                                .secondary()
                                .label(t!("Input.Replace"))
                                .disabled(!has_matches)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_next(window, cx);
                                })),
                        )
                        .child(
                            Button::new("replace-all")
                                .rounded(px(5.))
                                .small()
                                .secondary()
                                .label(t!("Input.Replace All"))
                                .disabled(!has_matches)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_all(window, cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    #[test]
    fn test_search() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", true);

        assert_eq!(matcher.len(), 3);
        let mut matches = matcher.clone();
        assert_eq!(matches.current_match_index(), 0);
        assert_eq!(matches.next(), Some(18..20));
        assert_eq!(matches.next(), Some(23..25));
        assert_eq!(matches.current_match_index(), 2);
        assert_eq!(matches.next(), Some(15..17));
        assert_eq!(matches.current_match_index(), 0);
        assert_eq!(matches.next_back(), Some(23..25));
        assert_eq!(matches.current_match_index(), 2);
        assert_eq!(matches.next_back(), Some(18..20));
        assert_eq!(matches.current_match_index(), 1);
        assert_eq!(matches.next_back(), Some(15..17));
        assert_eq!(matches.current_match_index(), 0);
        assert_eq!(matches.next_back(), Some(23..25));

        matcher.update_query("IS", false);
        assert_eq!(matcher.len(), 0);
        assert_eq!(matcher.next(), None);
        assert_eq!(matcher.next_back(), None);
    }

    #[test]
    fn test_search_label() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", true);
        assert_eq!(matcher.label(), "1/3");
        matcher.next();
        assert_eq!(matcher.label(), "2/3");
        matcher.next();
        assert_eq!(matcher.label(), "3/3");
        matcher.next();
        assert_eq!(matcher.label(), "1/3");

        matcher.update_query("IS", false);
        assert_eq!(matcher.label(), "0/0");
    }

    #[test]
    fn test_select_range_start() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from(".....aaaaa.....aaaaa.....aaaaa"));
        matcher.update_query("aaaaa", false);
        matcher.update_cursor_by_offset(0);
        assert_eq!(matcher.current_match_index(), 0);

        matcher.update_cursor_by_offset(5);
        assert_eq!(matcher.current_match_index(), 0);

        matcher.update_cursor_by_offset(12);
        assert_eq!(matcher.current_match_index(), 1);

        matcher.update_cursor_by_offset(16);
        assert_eq!(matcher.current_match_index(), 1);

        matcher.update_cursor_by_offset(30);
        assert_eq!(matcher.current_match_index(), 2);

        matcher.update_cursor_by_offset(31);
        assert_eq!(matcher.current_match_index(), 2);
    }

    #[test]
    fn test_next_scroll_direction_returns_down_without_wrap() {
        assert!(matches!(
            next_scroll_direction(0, 1),
            Some(MoveDirection::Down)
        ));
    }

    #[test]
    fn test_next_scroll_direction_returns_none_on_wrap() {
        assert!(next_scroll_direction(2, 0).is_none());
    }

    #[test]
    fn test_next_scroll_direction_returns_none_for_single_match() {
        assert!(next_scroll_direction(0, 0).is_none());
    }

    #[test]
    fn test_prev_scroll_direction_returns_up_without_wrap() {
        assert!(matches!(
            prev_scroll_direction(2, 1),
            Some(MoveDirection::Up)
        ));
    }

    #[test]
    fn test_prev_scroll_direction_returns_none_on_wrap() {
        assert!(prev_scroll_direction(0, 2).is_none());
    }

    #[test]
    fn test_prev_scroll_direction_returns_none_for_single_match() {
        assert!(prev_scroll_direction(0, 0).is_none());
    }
}
