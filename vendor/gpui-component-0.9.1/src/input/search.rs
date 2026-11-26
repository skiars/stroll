use gpui_base::input::{InputModeKind, SearchSession};
use ropey::Rope;
use rust_i18n::t;
use std::ops::Range;

use gpui::{
    App, AppContext as _, Context, Empty, Entity, FocusHandle, Focusable, Half, Hsla,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Pixels, Render,
    SharedString, StatefulInteractiveElement as _, Styled, Subscription, WeakEntity, Window,
    actions, div, prelude::FluentBuilder as _, px, rems,
};

use crate::{
    ActiveTheme, Colorize as _, Disableable, ElementExt, Icon, Selectable, Sizable,
    button::{Button, ButtonVariants},
    h_flex,
    input::{
        Enter, Escape, IndentInline, Input, InputBaseState, InputEvent, InputState, OutdentInline,
        Replace, TextareaState,
    },
    label::Label,
    popover::Popover,
    text::{RangeHighlight, TextViewState},
    tooltip::Tooltip,
    v_flex,
};

use gpui_kit_assets::IconName;

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
    fn has_advanced_find(&self) -> bool {
        false
    }
    fn can_find_in_selection(&self) -> bool {
        false
    }
    fn set_find_in_selection(&mut self, _: bool, _: &mut Context<Self>) {}
    fn can_replace(&self) -> bool {
        false
    }
    fn set_replace_mode(&mut self, _: bool, _: &mut Context<Self>) {}
    fn replace_match(&mut self, _: &str, _: bool, _: &mut Window, _: &mut Context<Self>) {}
}

impl<M: InputModeKind> SearchTarget for InputBaseState<M> {
    fn update_query(&mut self, query: &str, session: &mut SearchSession, cx: &mut Context<Self>) {
        self.set_find_query(query, session.case_insensitive, session.options, cx);
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

    fn has_advanced_find(&self) -> bool {
        InputBaseState::has_advanced_find(self)
    }

    fn can_find_in_selection(&self) -> bool {
        InputBaseState::can_find_in_selection(self)
    }

    fn set_find_in_selection(&mut self, enabled: bool, cx: &mut Context<Self>) {
        InputBaseState::set_find_in_selection(self, enabled, cx);
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

enum SearchField {
    Single(Entity<InputState>),
    Multi(Entity<TextareaState>),
}

impl SearchField {
    fn new(advanced: bool, placeholder: &str, window: &mut Window, cx: &mut App) -> Self {
        if advanced {
            Self::Multi(cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(placeholder.to_owned())
                    .find_field()
            }))
        } else {
            Self::Single(
                cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_owned())),
            )
        }
    }

    fn input(&self) -> Input {
        match self {
            Self::Single(field) => Input::new(field),
            Self::Multi(field) => Input::from_state(field.clone()),
        }
    }

    fn value(&self, cx: &App) -> gpui::SharedString {
        match self {
            Self::Single(field) => field.read(cx).value(),
            Self::Multi(field) => field.read(cx).value(),
        }
    }

    fn is_composing(&self, cx: &App) -> bool {
        match self {
            Self::Single(field) => field.read(cx).is_composing(),
            Self::Multi(field) => field.read(cx).is_composing(),
        }
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Single(field) => field.read(cx).focus_handle(cx),
            Self::Multi(field) => field.read(cx).focus_handle(cx),
        }
    }

    fn select_query(&self, query: &str, window: &mut Window, cx: &mut App) {
        fn select<M: InputModeKind>(
            field: &Entity<InputBaseState<M>>,
            query: Option<&str>,
            window: &mut Window,
            cx: &mut App,
        ) {
            field.update(cx, |field, cx| {
                if let Some(query) = query {
                    field.set_value(query, window, cx);
                }
                field.select_all(window, cx);
            });
        }
        match self {
            Self::Single(field) => select(field, (!query.is_empty()).then_some(query), window, cx),
            Self::Multi(field) => select(field, Some(query), window, cx),
        }
    }

    fn subscribe<T: SearchTarget>(
        &self,
        query: bool,
        cx: &mut Context<SearchPanel<T>>,
    ) -> Subscription {
        fn changed<T: SearchTarget>(
            panel: &mut SearchPanel<T>,
            query: bool,
            event: &InputEvent,
            cx: &mut Context<SearchPanel<T>>,
        ) {
            if matches!(event, InputEvent::Change) {
                if query {
                    panel.update_search_query(None, cx);
                } else {
                    cx.notify();
                }
            }
        }
        match self {
            Self::Single(field) => cx.subscribe(field, move |panel, _, event, cx| {
                changed(panel, query, event, cx)
            }),
            Self::Multi(field) => cx.subscribe(field, move |panel, _, event, cx| {
                changed(panel, query, event, cx)
            }),
        }
    }
}

#[doc(hidden)]
pub struct SearchPanel<T: SearchTarget> {
    target: WeakEntity<T>,
    search_input: SearchField,
    replace_input: SearchField,
    advanced: bool,
    session: gpui_base::input::SearchSession,
    panel_width: Option<Pixels>,

    _subscriptions: Vec<Subscription>,
}

impl<T: SearchTarget> SearchPanel<T> {
    pub(super) fn is_composing(&self, cx: &App) -> bool {
        self.search_input.is_composing(cx) || self.replace_input.is_composing(cx)
    }

    pub(super) fn replace_mode(&self) -> bool {
        self.session.replace_mode
    }

    pub(super) fn sync_session(&mut self, session: &gpui_base::input::SearchSession) {
        self.session = session.clone();
    }

    /// The query the panel's search input currently holds.
    pub(super) fn query(&self, cx: &App) -> gpui::SharedString {
        self.search_input.value(cx)
    }

    /// The panel's search input, so a test can assert what re-invoking search
    /// does to its value and selection.
    #[cfg(test)]
    pub(super) fn search_input(&self) -> &Entity<InputState> {
        match &self.search_input {
            SearchField::Single(field) => field,
            SearchField::Multi(_) => unreachable!("test uses a single-line target"),
        }
    }

    pub(crate) fn new(target: Entity<T>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let advanced = target.read(cx).has_advanced_find();
        let search_input = SearchField::new(advanced, "Find…", window, cx);
        let replace_input = SearchField::new(advanced, "Replace…", window, cx);

        cx.new(|cx| {
            let _subscriptions = vec![
                search_input.subscribe(true, cx),
                replace_input.subscribe(false, cx),
            ];

            Self {
                target: target.downgrade(),
                search_input,
                replace_input,
                advanced,
                session: gpui_base::input::SearchSession::default(),
                panel_width: None,
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
            self.search_input.focus_handle(cx).focus(window, cx);
        }

        self.search_input
            .select_query(&selected_text.to_string(), window, cx);

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
        let query = self.search_input.value(cx);
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
            && self.replace_input.focus_handle(cx).is_focused(window)
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
            cx.propagate();
            return;
        }

        let search = self.search_input.focus_handle(cx);
        let replace = self.replace_input.focus_handle(cx);
        if search.is_focused(window) {
            replace.focus(window, cx);
        } else if replace.is_focused(window) {
            search.focus(window, cx);
        } else {
            cx.propagate();
        }
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
            self.replace_input.focus_handle(cx)
        } else {
            self.search_input.focus_handle(cx)
        };
        focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Navigate the retained query in the current target.
    pub fn step(&mut self, previous: bool, cx: &mut Context<Self>) {
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

        let replacement = self.replace_input.value(cx);
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

        let replacement = self.replace_input.value(cx);
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
        if let SearchField::Single(input) = &self.search_input {
            input.update(cx, |input, cx| input.set_placeholder(label, window, cx));
        }
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

    #[doc(hidden)]
    pub fn retained_query(&self) -> &str {
        &self.session.query
    }

    /// Return to the document from its Find input. An old query cancels the
    /// in-progress search and is retained the next time the field opens.
    #[doc(hidden)]
    pub fn finish_modal_find(
        &mut self,
        restore_query: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.session.open || !self.has_focus(window, cx) || self.is_composing(cx) {
            return false;
        }
        if let Some(query) = restore_query {
            if let SearchField::Single(field) = &self.search_input {
                field.update(cx, |field, cx| field.set_value(query, window, cx));
            }
            self.update_search_query(None, cx);
            self.hide(window, cx);
        } else {
            _ = self.target.update(cx, |target, cx| target.focus_search_target(window, cx));
        }
        true
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
            .focus_handle(cx)
            .contains_focused(window, cx)
            || self
                .replace_input
                .focus_handle(cx)
                .contains_focused(window, cx)
    }
}

impl<T: SearchTarget> Focusable for SearchPanel<T> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_input.focus_handle(cx)
    }
}

fn find_button(
    id: &'static str,
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
    size: Pixels,
    icon_size: Pixels,
) -> Button {
    let label = label.into();
    Button::new(id)
        .rounded(px(4.))
        .with_size(size)
        .ghost()
        .compact()
        .icon(Icon::new(icon))
        .icon_size(icon_size)
        .accessibility_label(label.clone())
        .tooltip(label)
}

fn find_selected(button: Button, selected: bool, cx: &App) -> Button {
    button.selected(selected).when(selected, |button| {
        let theme = cx.theme();
        button
            .bg(theme.primary.mix_oklab(theme.popover, 0.33))
            .text_color(theme.primary.mix_oklab(theme.foreground, 0.35))
    })
}

fn find_field(
    input: Input,
    id: &'static str,
    label: impl Into<SharedString>,
    error: Option<String>,
    danger: Hsla,
) -> impl IntoElement {
    let label = label.into();
    let accessible_name = match &error {
        Some(error) => format!("{label}. {error}").into(),
        None => label,
    };
    let invalid = error.is_some();
    div()
        .id(id)
        .debug_selector(move || id.into())
        .flex_1()
        .min_w_0()
        .h(rems(2.))
        .when_some(error, |field, error| {
            field.tooltip(move |window, cx| Tooltip::new(error.clone()).build(window, cx))
        })
        .child(
            input
                .compact_field()
                .aria_label(accessible_name)
                .focus_bordered(!invalid)
                .w_full()
                .shadow_none()
                .when(invalid, |input| input.border_color(danger)),
        )
}

impl<T: SearchTarget> SearchPanel<T> {
    fn match_options(
        &self,
        entity: WeakEntity<Self>,
        size: Pixels,
        icon_size: Pixels,
        cx: &App,
    ) -> [Button; 3] {
        let case_panel = entity.clone();
        let word_panel = entity.clone();
        let letter_icon_size = icon_size + px(2.);
        [
            find_button(
                "find-case",
                Icon::default().data(include_bytes!("icons/find-match-case.svg")),
                t!("Input.Find match case"),
                size,
                letter_icon_size,
            )
            .map(|button| find_selected(button, !self.session.case_insensitive, cx))
            .toggled(!self.session.case_insensitive)
            .on_click(move |_, _, cx| {
                _ = case_panel.update(cx, |panel, cx| {
                    panel.session.case_insensitive = !panel.session.case_insensitive;
                    panel.update_search_query(None, cx);
                });
            }),
            find_button(
                "find-whole-word",
                Icon::default().data(include_bytes!("icons/find-whole-word.svg")),
                t!("Input.Find whole word"),
                size,
                letter_icon_size,
            )
            .map(|button| find_selected(button, self.session.options.whole_word, cx))
            .toggled(self.session.options.whole_word)
            .on_click(move |_, _, cx| {
                _ = word_panel.update(cx, |panel, cx| {
                    panel.session.options.whole_word = !panel.session.options.whole_word;
                    panel.update_search_query(None, cx);
                });
            }),
            find_button(
                "find-regex",
                IconName::Regex,
                t!("Input.Find regex"),
                size,
                icon_size,
            )
            .map(|button| find_selected(button, self.session.options.regex, cx))
            .toggled(self.session.options.regex)
            .on_click(move |_, _, cx| {
                _ = entity.update(cx, |panel, cx| {
                    panel.session.options.regex = !panel.session.options.regex;
                    panel.update_search_query(None, cx);
                });
            }),
        ]
    }
}

impl<T: SearchTarget> Render for SearchPanel<T> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.session.open {
            return Empty.into_any_element();
        }

        let allow_replace = self.replaceable(cx);
        let replace_open = allow_replace && self.session.replace_mode;
        self.session.replace_mode = replace_open;
        let scope_available = self
            .target
            .read_with(cx, |target, _| target.can_find_in_selection())
            .unwrap_or(false);
        let scope_missing = self.session.in_selection && !scope_available;
        let matcher = &self.session.matcher;
        let has_matches = !matcher.is_empty();
        let search_error = matcher.error().map(str::to_owned);
        let replacement_error = if replace_open && has_matches {
            matcher.replacement_error(&self.replace_input.value(cx))
        } else {
            None
        };
        let can_replace = has_matches && search_error.is_none() && replacement_error.is_none();
        let replacement_error = replacement_error.or_else(|| {
            self.session
                .replacement_failure(&self.replace_input.value(cx))
                .map(str::to_owned)
        });

        let icon_size = cx.theme().control_font_size;
        let button_size = (window.rem_size() * 2. - px(4.)).max(icon_size + px(2.));
        let gap = px(3.);
        let side_padding = px(8.);
        let count_chars = matcher.len().to_string().len() * 2 + 1;
        let count_width = icon_size * (count_chars as f32 * 0.62).max(3.3);
        let leading_buttons = u8::from(allow_replace) + u8::from(self.advanced);
        let before_navigation = count_width + (button_size + gap) * f32::from(leading_buttons);
        let expanded_width =
            before_navigation + (button_size + gap) * 6. + side_padding * 2. + icon_size * 5.;
        let narrow = self.advanced && self.panel_width.is_some_and(|width| width < expanded_width);
        let danger = cx.theme().danger;
        let progress_color = if search_error.is_some() {
            danger
        } else {
            cx.theme().muted_foreground
        };
        let entity = cx.weak_entity();
        let options = if narrow {
            let active = !self.session.case_insensitive
                || self.session.options.whole_word
                || self.session.options.regex;
            let popover = Popover::new("find-options-popup")
                .trigger(
                    find_button(
                        "find-options",
                        IconName::Ellipsis,
                        t!("Input.Find options"),
                        button_size,
                        icon_size,
                    )
                    .map(|button| find_selected(button, active, cx)),
                )
                .content(move |_, _, cx| {
                    let buttons = entity
                        .read_with(cx, |panel, cx| {
                            panel.match_options(entity.clone(), button_size, icon_size, cx)
                        })
                        .ok();
                    h_flex()
                        .id("find-options-actions")
                        .debug_selector(|| "find-options-actions".into())
                        .gap(gap)
                        .children(buttons.into_iter().flatten())
                });
            div()
                .debug_selector(|| "find-options-trigger".into())
                .child(popover)
                .into_any_element()
        } else {
            let options = self.match_options(entity, button_size, icon_size, cx);
            let count = if self.advanced { 3 } else { 1 };
            h_flex()
                .gap(px(2.))
                .children(options.into_iter().take(count))
                .into_any_element()
        };

        v_flex()
            .id("search-panel")
            .debug_selector(|| "find-bar".into())
            .occlude()
            .track_focus(&self.focus_handle(cx))
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_action_enter))
            .on_action(cx.listener(Self::on_action_escape))
            .on_action(cx.listener(Self::on_action_tab))
            .on_action(cx.listener(Self::on_action_tab_prev))
            .on_action(cx.listener(Self::on_action_replace))
            .on_prepaint({
                let entity = cx.weak_entity();
                move |bounds, _, cx| {
                    _ = entity.update(cx, |panel, cx| {
                        if panel.panel_width != Some(bounds.size.width) {
                            panel.panel_width = Some(bounds.size.width);
                            cx.notify();
                        }
                    });
                }
            })
            .font_family(cx.theme().font_family.clone())
            .py(px(8.))
            .px(side_padding)
            .w_full()
            .gap(px(4.))
            .bg(cx.theme().tokens.popover)
            .border_b_1()
            .rounded(cx.theme().radius.half())
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap(gap)
                    .child(find_field(
                        self.search_input.input().suffix(options),
                        "find-query-field",
                        t!("Input.Find"),
                        search_error,
                        danger,
                    ))
                    .when(allow_replace, |row| {
                        row.child(
                            find_button(
                                "replace-mode",
                                IconName::PanelTopOpen,
                                t!("Input.Find toggle replace"),
                                button_size,
                                icon_size,
                            )
                            .map(|button| find_selected(button, replace_open, cx))
                            .toggled(replace_open)
                            .on_click(cx.listener(
                                |this, _, window, cx| this.toggle_replace_mode(window, cx),
                            )),
                        )
                    })
                    .when(self.advanced, |row| {
                        let scope_label = if scope_missing {
                            t!("Input.Find selection missing")
                        } else {
                            t!("Input.Find selection description")
                        };
                        row.child(
                            find_button(
                                "find-in-selection",
                                IconName::ScanText,
                                scope_label,
                                button_size,
                                icon_size,
                            )
                            .map(|button| find_selected(button, self.session.in_selection, cx))
                            .toggled(self.session.in_selection)
                            .disabled(!scope_available && !self.session.in_selection)
                            .when(scope_missing, |button| button.text_color(danger))
                            .on_click(cx.listener(|this, _, _, cx| {
                                let enabled = !this.session.in_selection;
                                _ = this.target.update(cx, |target, cx| {
                                    target.set_find_in_selection(enabled, cx)
                                });
                                this.session.in_selection = enabled;
                                cx.notify();
                            })),
                        )
                    })
                    .child(
                        Label::new(matcher.label())
                            .w(count_width)
                            .text_size(icon_size)
                            .text_center()
                            .text_color(progress_color)
                            .flex_shrink_0(),
                    )
                    .child(
                        find_button(
                            "prev",
                            IconName::ChevronUp,
                            t!("Input.Find previous match"),
                            button_size,
                            icon_size,
                        )
                        .disabled(!has_matches)
                        .on_click(cx.listener(|this, _, _, cx| this.step(true, cx))),
                    )
                    .child(
                        find_button(
                            "next",
                            IconName::ChevronDown,
                            t!("Input.Find next match"),
                            button_size,
                            icon_size,
                        )
                        .disabled(!has_matches)
                        .on_click(cx.listener(|this, _, _, cx| this.step(false, cx))),
                    )
                    .child(
                        find_button(
                            "close",
                            IconName::Close,
                            t!("Input.Find close"),
                            button_size,
                            icon_size,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.hide(window, cx))),
                    ),
            )
            .when(replace_open, |panel| {
                panel.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap(gap)
                        .child(find_field(
                            self.replace_input.input(),
                            "find-replacement-field",
                            t!("Input.Replace"),
                            replacement_error,
                            danger,
                        ))
                        .child(div().w(before_navigation).flex_shrink_0())
                        .child(
                            find_button(
                                "replace-one",
                                IconName::Replace,
                                t!("Input.Find replace current"),
                                button_size,
                                icon_size,
                            )
                            .disabled(!can_replace)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.replace_next(window, cx)),
                            ),
                        )
                        .child(
                            find_button(
                                "replace-all",
                                IconName::ReplaceAll,
                                t!("Input.Replace All"),
                                button_size,
                                icon_size,
                            )
                            .disabled(!can_replace)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.replace_all(window, cx)),
                            ),
                        )
                        .child(div().w(button_size).flex_shrink_0()),
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
