use super::*;
use crate::input::{DiagnosticEntry, EditorMode};
use std::ops::Range;

use lsp_types::{CompletionItem, CompletionTextEdit, Hover, Range as LspRange};

#[derive(Clone, Debug, Default)]
pub struct CompletionMenuState {
    pub open: bool,
    pub trigger_start_offset: Option<usize>,
    pub query: String,
    pub items: Vec<CompletionItem>,
    pub(super) open_before_edit: bool,
    pub(super) document_revision: u64,
    pub(super) caret: usize,
    revision: u64,
}

impl CompletionMenuState {
    #[doc(hidden)]
    pub fn origin(&self) -> (u64, usize) {
        (self.document_revision, self.caret)
    }

    /// Bumped whenever the content changes.
    ///
    /// A renderer that mirrors this menu compares revisions to decide whether
    /// to rebuild, so it never has to compare the item list itself.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

#[derive(Clone, Debug, Default)]
pub struct CodeActionMenuState {
    pub open: bool,
    pub range: Range<usize>,
    pub hover: Option<HoverPopoverState>,
    pub loading: bool,
    pub error: Option<SharedString>,
    pub preview: Option<(CodeActionItem, SharedString)>,
    pub items: Vec<CodeActionItem>,
    revision: u64,
}

impl CodeActionMenuState {
    /// Bumped whenever the content changes. See [`CompletionMenuState::revision`].
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

#[derive(Clone, Debug)]
pub struct HoverPopoverState {
    pub symbol_range: Range<usize>,
    pub hover: Option<Hover>,
    pub documentation: Option<HoverDocumentation>,
    pub diagnostics: Rc<[DiagnosticEntry]>,
    pub(super) target_range: Range<usize>,
    pub(super) pointer: Option<gpui::Point<gpui::Pixels>>,
}

impl HoverPopoverState {
    pub(super) fn visible(&self) -> bool {
        self.hover.is_some() || !self.diagnostics.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ContextMenuContent {
    pub(crate) completion: CompletionMenuState,
    pub(crate) code_action: CodeActionMenuState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputOverlayKind {
    Completion,
    CodeAction,
}

impl InputBaseState<EditorMode> {
    pub fn present_completion_items(
        &mut self,
        trigger_start_offset: usize,
        query: impl Into<String>,
        items: Vec<CompletionItem>,
        cx: &mut Context<Self>,
    ) {
        let caret = self.cursor();
        let menu = &mut self.extras.context_menu_content.completion;
        menu.trigger_start_offset = Some(trigger_start_offset);
        menu.query = query.into();
        menu.items = items;
        menu.open = !menu.items.is_empty();
        menu.document_revision = self.document_revision;
        menu.caret = caret;
        menu.bump();
        cx.notify();
    }

    pub fn present_code_actions(&mut self, items: Vec<CodeActionItem>, cx: &mut Context<Self>) {
        self.extras.context_menu_content.code_action.range = self.selected_range();
        self.extras.context_menu_content.code_action.items = items;
        self.extras.context_menu_content.code_action.open = !self
            .extras
            .context_menu_content
            .code_action
            .items
            .is_empty();
        self.extras.context_menu_content.code_action.bump();
        cx.notify();
    }

    pub fn present_hover(
        &mut self,
        symbol_range: Range<usize>,
        hover: Hover,
        cx: &mut Context<Self>,
    ) {
        self.extras.lsp._hover_task = Task::ready(Ok(()));
        let diagnostics = self.extras.hover_popover.as_ref().and_then(|popover| {
            (popover.symbol_range == symbol_range).then(|| popover.diagnostics.clone())
        });
        self.extras.hover_popover = Some(HoverPopoverState {
            target_range: symbol_range.clone(),
            pointer: None,
            symbol_range,
            hover: Some(hover),
            documentation: None,
            diagnostics: diagnostics.unwrap_or_default(),
        });
        cx.notify();
    }

    pub fn present_diagnostic(&mut self, diagnostic: DiagnosticEntry, cx: &mut Context<Self>) {
        let range = diagnostic.range.clone();
        let popover = self
            .extras
            .hover_popover
            .get_or_insert_with(|| HoverPopoverState {
                target_range: range.clone(),
                pointer: None,
                symbol_range: range,
                hover: None,
                documentation: None,
                diagnostics: Rc::default(),
            });
        popover.diagnostics = Rc::from([diagnostic]);
        cx.notify();
    }

    pub fn diagnostic_popover(&self) -> Option<Rc<DiagnosticEntry>> {
        Some(Rc::new(self.hover_popover()?.diagnostics.first()?.clone()))
    }

    pub fn clear_diagnostic_popover(&mut self, cx: &mut Context<Self>) {
        if let Some(popover) = self.extras.hover_popover.as_mut()
            && !popover.diagnostics.is_empty()
        {
            if popover.hover.is_none() {
                self.clear_hover_state(cx);
            } else {
                popover.diagnostics = Rc::default();
                cx.notify();
            }
        }
    }

    pub fn route_overlay_action(
        &mut self,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.handle_action_for_context_menu(action, window, cx)
    }

    pub fn set_overlay_action_handler(
        &mut self,
        handler: impl Fn(
            InputOverlayKind,
            Box<dyn gpui::Action>,
            &mut Window,
            &mut Context<InputBaseState<EditorMode>>,
        ) -> bool
        + 'static,
    ) {
        self.overlay_action_handler = Some(Rc::new(handler));
    }

    pub fn has_overlay_action_handler(&self) -> bool {
        self.overlay_action_handler.is_some()
    }

    pub fn dismiss_completion_overlay(&mut self, cx: &mut Context<Self>) {
        if self.extras.context_menu_content.completion.open {
            self.extras.context_menu_content.completion.open = false;
            if let Some(provider) = &self.extras.lsp.completion_provider {
                provider.completion_dismissed();
            }
            self.extras.context_menu_task = Task::ready(Ok(()));
            cx.notify();
        }
    }

    pub fn dismiss_code_action_overlay(&mut self, cx: &mut Context<Self>) {
        if self.extras.context_menu_content.code_action.open {
            self.extras.context_menu_content.code_action.open = false;
            self.extras.context_menu_task = Task::ready(Ok(()));
            self.clear_hover_state(cx);
            cx.notify();
        }
    }

    pub fn insert_completion(
        &mut self,
        item: &CompletionItem,
        fallback_range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_editable()
            || self.is_composing()
            || !self.selected_range().is_empty()
            || !self.selections.is_single()
        {
            return;
        }
        let Some((primary, edits)) = completion_edits(&self.text, item, fallback_range) else {
            return;
        };
        self.replace_text_in_ranges_focusing(&edits, Some(&primary), window, cx);
        self.focus(window, cx);
    }

    #[doc(hidden)]
    pub fn completion_menu_state(&self) -> &CompletionMenuState {
        &self.extras.context_menu_content.completion
    }

    #[doc(hidden)]
    pub fn code_action_menu_state(&self) -> &CodeActionMenuState {
        &self.extras.context_menu_content.code_action
    }

    pub fn code_action_hover(&self) -> Option<HoverPopoverState> {
        let menu = self.code_action_menu_state();
        menu.open.then(|| {
            menu.hover.clone().unwrap_or_else(|| HoverPopoverState {
                symbol_range: menu.range.start..menu.range.start,
                target_range: menu.range.clone(),
                pointer: None,
                hover: None,
                documentation: None,
                diagnostics: Rc::default(),
            })
        })
    }

    pub fn hover_popover(&self) -> Option<&HoverPopoverState> {
        self.extras
            .hover_popover
            .as_ref()
            .filter(|popover| popover.visible())
    }

    pub fn dismiss_lsp_overlays(&mut self, cx: &mut Context<Self>) {
        self.hide_context_menu(cx);
        self.clear_hover_state(cx);
    }
}

fn completion_edits(
    text: &Rope,
    item: &CompletionItem,
    fallback: Range<usize>,
) -> Option<(Range<usize>, Vec<(Range<usize>, String)>)> {
    let to_bytes = |range: &LspRange| {
        let start = text.position_to_offset(&range.start);
        let end = text.position_to_offset(&range.end);
        (start <= end
            && text.offset_to_position(start) == range.start
            && text.offset_to_position(end) == range.end)
            .then_some(start..end)
    };
    let (range, replacement) = match &item.text_edit {
        Some(CompletionTextEdit::Edit(edit)) => (to_bytes(&edit.range)?, edit.new_text.as_str()),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => {
            (to_bytes(&edit.replace)?, edit.new_text.as_str())
        }
        None => (fallback, item.insert_text.as_deref().unwrap_or(&item.label)),
    };
    let mut edits = vec![(range.clone(), replacement.to_owned())];
    for edit in item.additional_text_edits.iter().flatten() {
        edits.push((to_bytes(&edit.range)?, edit.new_text.clone()));
    }

    for (range, replacement) in &mut edits {
        *replacement = replacement.replace("\r\n", "\n");
        if range.start > range.end
            || range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
            || replacement.contains(['\r', '\0'])
        {
            return None;
        }
    }
    edits.sort_by_key(|(range, _)| (range.start, range.end));
    if edits
        .windows(2)
        .any(|pair| pair[0].0.end > pair[1].0.start || pair[0].0.start == pair[1].0.start)
    {
        return None;
    }
    Some((range, edits))
}
