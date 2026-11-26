use anyhow::Result;
use gpui::{App, Context, MouseMoveEvent, Pixels, Point, SharedString, Task, Window};
use instant::Duration;
use ropey::Rope;
use std::{collections::BTreeMap, ops::Range, rc::Rc, sync::Arc, task::Poll};

use crate::input::{
    DiagnosticEntry, DiagnosticSeverity, EditorMode, HoverPopoverState, InputBaseState, RopeExt,
};

use super::super::diagnostics::adjust_diagnostic_range;

/// Additional documentation resolved after the original hover is visible.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HoverDocumentation {
    /// Paired Markdown paragraphs keyed by the byte offset of the original block.
    pub paragraphs: Arc<BTreeMap<usize, HoverParagraph>>,
    /// Optional additional prose for literal LSP plaintext.
    pub plaintext: Option<SharedString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoverParagraph {
    pub original: SharedString,
    pub translation: SharedString,
}

/// Hover provider
///
/// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_hover
pub trait HoverProvider {
    /// Returns the active workspace's overrides for code fence languages.
    fn code_languages(&self) -> Option<Arc<BTreeMap<SharedString, SharedString>>> {
        None
    }

    /// textDocument/hover
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_hover
    fn hover(
        &self,
        _text: &Rope,
        _offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>>;

    /// Optionally supply a second, asynchronous update to the same hover.
    fn enrich_hover(
        &self,
        _hover: &lsp_types::Hover,
        _offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Option<HoverDocumentation>> {
        Task::ready(None)
    }
}

const HOVER_DELAY: Duration = Duration::from_millis(300);

impl InputBaseState<EditorMode> {
    fn documentation_target_at(&self, offset: usize) -> Option<Range<usize>> {
        self.text.char_at(offset).filter(|c| !c.is_whitespace())?;
        Some(
            self.text
                .word_range(offset)
                .filter(|word| word.contains(&offset))
                .unwrap_or(offset..offset),
        )
    }

    fn hover_diagnostics_at(&self, offset: usize) -> Vec<&DiagnosticEntry> {
        let mut diagnostics = self
            .diagnostics()
            .into_iter()
            .flat_map(|set| set.at_offset(offset))
            .collect::<Vec<_>>();
        diagnostics.sort_by_key(|entry| {
            let severity = match entry.severity {
                DiagnosticSeverity::Error => 0,
                DiagnosticSeverity::Warning => 1,
                DiagnosticSeverity::Info => 2,
                DiagnosticSeverity::Hint => 3,
            };
            (severity, entry.range.len())
        });
        diagnostics
    }

    fn diagnostic_hover_symbol(
        &self,
        target: &Range<usize>,
        offset: usize,
        word: Range<usize>,
    ) -> Range<usize> {
        // A multiline diagnostic can begin or end outside the viewport.
        // Anchor to the word when the full range cannot fit on the pointer row.
        if self.range_to_bounds(target).is_some_and(|bounds| {
            self.range_to_bounds(&(offset..offset))
                .is_some_and(|caret| bounds.size.height == caret.size.height)
        }) {
            target.clone()
        } else {
            word
        }
    }

    /// Request documentation at the active editor caret.
    #[doc(hidden)]
    pub fn show_hover_at_caret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing() || self.is_context_menu_open(cx) {
            return;
        }
        let offset = self.cursor();
        let Some((provider, target)) = self
            .extras
            .lsp
            .hover_provider
            .clone()
            .zip(self.documentation_target_at(offset))
        else {
            return;
        };

        self.extras.dismissed_hover_target = None;
        if let Some(popover) = self.extras.hover_popover.as_mut()
            && popover.documentation_target.as_ref() == Some(&target)
            && (popover.hover.is_some() || popover.keyboard_caret == Some(offset))
        {
            popover.keyboard_caret = Some(offset);
            popover.pointer = None;
            return;
        }

        let diagnostics = self
            .hover_diagnostics_at(offset)
            .into_iter()
            .cloned()
            .collect();
        self.request_language_hover(
            offset,
            HoverPopoverState {
                symbol_range: target.clone(),
                target_range: target.clone(),
                documentation_target: Some(target),
                pointer: None,
                keyboard_caret: Some(offset),
                diagnostics,
                hover: None,
                documentation: None,
                waiting_for_delay: true,
                waiting_for_documentation: false,
                presented: Default::default(),
                pointer_handoff: Default::default(),
            },
            Some(provider),
            window,
            cx,
        );
    }

    /// Handle a pointer-triggered hover request.
    pub(super) fn handle_hover_popover(
        &mut self,
        offset: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<InputBaseState<EditorMode>>,
    ) {
        if self.selecting {
            return;
        }
        if self
            .extras
            .dismissed_hover_target
            .as_ref()
            .is_some_and(|range| range.contains(&offset) || range.start == offset)
        {
            return;
        }
        self.extras.dismissed_hover_target = None;

        let diagnostics = self.hover_diagnostics_at(offset);

        if self.extras.hover_popover.as_ref().is_some_and(|popover| {
            popover.pointer.is_none()
                && (popover.target_range.contains(&offset) || popover.target_range.start == offset)
                && diagnostics.iter().copied().eq(popover.diagnostics.iter())
        }) {
            return;
        }

        let provider = self.extras.lsp.hover_provider.clone();
        let documentation_target = provider
            .as_ref()
            .and_then(|_| self.documentation_target_at(offset));
        if diagnostics.is_empty() && documentation_target.is_none() {
            self.clear_language_hover(cx);
            return;
        }

        let word = documentation_target
            .clone()
            .or_else(|| self.text.word_range(offset))
            .unwrap_or(offset..offset);
        let target_range = diagnostics
            .first()
            .map_or_else(|| word.clone(), |entry| entry.range.clone());
        let symbol_range = if diagnostics.is_empty() {
            word
        } else {
            self.diagnostic_hover_symbol(&target_range, offset, word)
        };
        if let Some(popover) = self.extras.hover_popover.as_ref()
            && popover.documentation_target == documentation_target
            && (documentation_target.is_some() || popover.target_range == target_range)
        {
            let changed_diagnostics = (!diagnostics.iter().copied().eq(popover.diagnostics.iter()))
                .then(|| diagnostics.into_iter().cloned().collect());
            let popover = self.extras.hover_popover.as_mut().unwrap();
            popover.pointer = Some(position);
            if let Some(diagnostics) = changed_diagnostics {
                popover.diagnostics = diagnostics;
                popover.target_range = target_range;
                popover.symbol_range = symbol_range;
                cx.notify();
            }
            return;
        }

        let diagnostics: Rc<[_]> = diagnostics.into_iter().cloned().collect();
        let provider = provider.filter(|_| documentation_target.is_some());
        self.request_language_hover(
            offset,
            HoverPopoverState {
                symbol_range,
                target_range,
                documentation_target,
                pointer: Some(position),
                keyboard_caret: None,
                diagnostics,
                hover: None,
                documentation: None,
                waiting_for_delay: true,
                waiting_for_documentation: false,
                presented: Default::default(),
                pointer_handoff: Default::default(),
            },
            provider,
            window,
            cx,
        );
    }

    fn request_language_hover(
        &mut self,
        offset: usize,
        mut popover: HoverPopoverState,
        provider: Option<Rc<dyn HoverProvider>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let documentation_target = popover.documentation_target.clone();
        let from_pointer = popover.pointer.is_some();
        if self.hover_popover().is_some() {
            cx.notify();
        }
        popover.waiting_for_documentation = provider.is_some();
        self.extras.hover_popover = Some(popover);

        // Replacing the task cancels the previous delay, request and enrichment.
        let text = self.text.clone();
        let revision = self.document_revision;
        self.extras.lsp._hover_task = cx.spawn_in(window, async move |this, cx| {
            let reveal = from_pointer.then(|| cx.background_executor().timer(HOVER_DELAY));
            let mut request = if let Some(provider) = provider.as_ref() {
                if from_pointer {
                    // Give the server a head start without querying briefly crossed symbols.
                    cx.background_executor().timer(HOVER_DELAY / 2).await;
                }
                this.update_in(cx, |editor, window, cx| {
                    editor.validate_language_hover(cx);
                    (editor.document_revision == revision
                        && editor.extras.hover_popover.as_ref().is_some_and(|popover| {
                            popover.documentation_target == documentation_target
                        }))
                    .then(|| provider.hover(&text, offset, window, cx))
                })?
            } else {
                None
            };
            let mut early_hover = None;
            if let Some(reveal) = reveal {
                reveal.await;
                if let Some(task) = request.as_mut()
                    && let Poll::Ready(result) = futures::poll!(task)
                {
                    request = None;
                    early_hover = result.ok().flatten();
                }
                this.update_in(cx, |editor, _, cx| {
                    editor.validate_language_hover(cx);
                    if let Some(popover) = editor.extras.hover_popover.as_mut()
                        && editor.document_revision == revision
                    {
                        popover.waiting_for_delay = false;
                        if popover.visible()
                            && (early_hover.is_none()
                                || popover.documentation_target != documentation_target)
                        {
                            cx.notify();
                        }
                    }
                })?;
            }
            let Some(provider) = provider else {
                return Ok(());
            };
            let hover = match (early_hover, request) {
                (None, Some(request)) => request.await.ok().flatten(),
                (hover, _) => hover,
            };
            let Some(hover) = hover else {
                this.update_in(cx, |editor, _, _| {
                    if editor.document_revision == revision
                        && let Some(popover) = editor.extras.hover_popover.as_mut()
                        && popover.documentation_target == documentation_target
                    {
                        popover.waiting_for_documentation = false;
                        if popover.is_keyboard_triggered() {
                            editor.extras.hover_popover = None;
                        }
                    }
                })?;
                return Ok(());
            };

            let enrichment = this.update_in(cx, |editor, window, cx| {
                editor.validate_language_hover(cx);
                if editor.document_revision != revision {
                    return None;
                }
                let popover = editor.extras.hover_popover.as_mut()?;
                if popover.documentation_target != documentation_target {
                    return None;
                }
                if popover.diagnostics.is_empty()
                    && let Some(range) = hover.range
                {
                    let start = editor.text.position_to_offset(&range.start);
                    let end = editor.text.position_to_offset(&range.end);
                    if start <= offset && offset < end {
                        popover.target_range = start..end;
                        popover.symbol_range = start..end;
                    }
                }
                let enrichment = provider.enrich_hover(&hover, offset, window, cx);
                popover.waiting_for_delay = false;
                popover.waiting_for_documentation = false;
                popover.hover = Some(hover.clone());
                editor.update_action_hover_documentation();
                cx.notify();
                Some(enrichment)
            })?;
            if let Some(enrichment) = enrichment
                && let Some(documentation) = enrichment.await
            {
                this.update_in(cx, |editor, _, cx| {
                    editor.validate_language_hover(cx);
                    if editor.document_revision == revision
                        && let Some(popover) = editor.extras.hover_popover.as_mut()
                        && popover.hover.as_ref() == Some(&hover)
                    {
                        popover.documentation = Some(documentation);
                        editor.update_action_hover_documentation();
                        cx.notify();
                    }
                })?;
            }
            Ok(())
        });
    }

    pub(crate) fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.hover_popover().is_some_and(|hover| {
            hover.is_keyboard_triggered() || hover.pointer_handoff.take() == Some(event.position)
        }) || self.has_pending_keyboard_hover()
        {
            return;
        }
        if !self.visible_text_bounds.contains(&event.position) {
            self.clear_hover_state(cx);
            return;
        }
        if event.modifiers.alt {
            self.clear_hover_state(cx);
            return;
        }
        if event.modifiers.secondary(cx) {
            self.clear_language_hover(cx);
            let (offset, _) = self.index_for_mouse_position(event.position);
            self.handle_hover_definition(offset, window, cx);
        } else {
            // Repaint only when the definition underline goes away; the hover
            // and definition results notify when they arrive.
            if !self.extras.hover_definition.is_empty() {
                self.extras.hover_definition.clear();
                cx.notify();
            }
            if !window.is_pointer_motion() {
                return;
            }
            if let Some(offset) = self.text_offset_at_point(event.position) {
                self.handle_hover_popover(offset, event.position, window, cx);
            } else {
                self.extras.dismissed_hover_target = None;
                self.clear_language_hover(cx);
            }
        }
    }

    /// Marks the current capture-phase mouse move as a handoff to this editor's popup.
    #[doc(hidden)]
    pub fn set_hover_pointer_handoff(
        &self,
        symbol_range: &Range<usize>,
        position: Option<Point<Pixels>>,
    ) {
        if let Some(hover) = self.hover_popover()
            && hover.symbol_range == *symbol_range
        {
            hover.pointer_handoff.set(position);
        }
    }

    pub(in crate::input) fn adjust_language_hover_for_edit(
        &mut self,
        edited: &Range<usize>,
        inserted_len: usize,
    ) {
        self.extras.lsp._hover_task = Task::ready(Ok(()));
        self.extras.dismissed_hover_target = None;
        self.extras.hover_definition.clear();
        let Some(popover) = self.extras.hover_popover.as_mut() else {
            return;
        };
        // Documentation belongs to an unchanged symbol. Diagnostic ranges can
        // survive partial edits and are refreshed against the laid-out text.
        let remap = |range: &Range<usize>| adjust_diagnostic_range(range, edited, inserted_len);
        let Some(target_range) = remap(&popover.target_range) else {
            self.extras.hover_popover = None;
            return;
        };
        if popover.waiting_for_delay || popover.pointer.is_none() || popover.diagnostics.is_empty()
        {
            self.extras.hover_popover = None;
            return;
        }
        popover.symbol_range = remap(&popover.symbol_range).unwrap_or_else(|| target_range.clone());
        popover.target_range = target_range;
        popover.documentation_target = popover.documentation_target.as_ref().and_then(|range| {
            (popover.hover.is_some() && (range.end <= edited.start || range.start >= edited.end))
                .then(|| remap(range))
                .flatten()
        });
        if popover.documentation_target.is_none() {
            popover.hover = None;
            popover.documentation = None;
        }
    }

    pub(crate) fn has_pending_keyboard_hover(&self) -> bool {
        self.extras
            .hover_popover
            .as_ref()
            .is_some_and(|popover| popover.is_keyboard_triggered() && !popover.presented.get())
    }

    /// Whether Escape will be handled by the shared editor hover.
    #[doc(hidden)]
    pub fn has_language_hover(&self) -> bool {
        self.active_language_hover().is_some()
    }

    fn active_language_hover(&self) -> Option<&HoverPopoverState> {
        self.extras.hover_popover.as_ref().filter(|popover| {
            popover.visible() || popover.waiting_for_delay || popover.waiting_for_documentation
        })
    }

    pub(crate) fn has_pending_pointer_hover(&self) -> bool {
        self.extras.hover_popover.as_ref().is_some_and(|popover| {
            popover.pointer.is_some() && (!popover.visible() || !popover.presented.get())
        })
    }

    /// Exclude the part of a projected editor covered by its pinned file headers.
    /// Called in prepaint after the text layout and before the deferred popups.
    #[doc(hidden)]
    pub fn clip_text_viewport_top(&mut self, top: Pixels, cx: &mut Context<Self>) {
        let bounds = &mut self.visible_text_bounds;
        let top = top.clamp(bounds.top(), bounds.bottom());
        bounds.size.height = bounds.bottom() - top;
        bounds.origin.y = top;
        self.validate_language_hover(cx);
    }

    pub(crate) fn validate_language_hover(&mut self, cx: &mut Context<Self>) {
        let Some(popover) = self.extras.hover_popover.as_ref() else {
            return;
        };
        if let Some(caret) = popover.keyboard_caret {
            if self.cursor() != caret || self.visible_text_range_bounds(&(caret..caret)).is_none() {
                self.clear_language_hover(cx);
            }
            return;
        }
        let Some(pointer) = popover.pointer else {
            return;
        };
        let offset = self
            .visible_text_bounds
            .contains(&pointer)
            .then(|| self.text_offset_at_point(pointer))
            .flatten();
        let Some(offset) = offset.filter(|_| {
            self.visible_text_range_bounds(&popover.symbol_range)
                .is_some()
        }) else {
            self.clear_language_hover(cx);
            return;
        };
        let same_documentation_target = popover
            .documentation_target
            .as_ref()
            .is_some_and(|target| self.documentation_target_at(offset).as_ref() == Some(target));
        let same_diagnostic_target = !popover.diagnostics.is_empty()
            && (popover.target_range.contains(&offset) || popover.target_range.start == offset);
        if !same_documentation_target && !same_diagnostic_target {
            self.clear_language_hover(cx);
            return;
        }
        let diagnostics = self.hover_diagnostics_at(offset);
        let diagnostics_changed = !diagnostics.iter().copied().eq(popover.diagnostics.iter());
        if !diagnostics_changed
            && (same_documentation_target || popover.documentation_target.is_none())
        {
            return;
        }
        let target = diagnostics.first().map(|diagnostic| {
            let target = diagnostic.range.clone();
            let word = self.text.word_range(offset).unwrap_or(offset..offset);
            let symbol = self.diagnostic_hover_symbol(&target, offset, word);
            (target, symbol)
        });
        let diagnostics = diagnostics_changed.then(|| diagnostics.into_iter().cloned().collect());
        let popover = self.extras.hover_popover.as_mut().unwrap();
        if let Some(diagnostics) = diagnostics {
            popover.diagnostics = diagnostics;
        }
        if let Some((target, symbol)) = target {
            popover.target_range = target;
            popover.symbol_range = symbol;
        } else if same_documentation_target
            && let Some(target) = popover.documentation_target.clone()
        {
            popover.target_range = target.clone();
            popover.symbol_range = target;
        }
        if !same_documentation_target {
            popover.documentation_target = None;
            popover.waiting_for_documentation = false;
            popover.hover = None;
            popover.documentation = None;
        }
        if popover.diagnostics.is_empty() && popover.documentation_target.is_none() {
            self.clear_language_hover(cx);
        }
        cx.notify();
    }

    pub(crate) fn dismiss_language_hover(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(popover) = self.active_language_hover() else {
            return false;
        };
        let target = popover
            .documentation_target
            .as_ref()
            .unwrap_or(&popover.target_range);
        let pointer = window.mouse_position();
        if self.visible_text_bounds.contains(&pointer)
            && self
                .text_offset_at_point(pointer)
                .is_some_and(|offset| target.contains(&offset) || target.start == offset)
        {
            self.extras.dismissed_hover_target = Some(target.clone());
        }
        self.clear_language_hover(cx);
        true
    }

    pub fn clear_hover_state(&mut self, cx: &mut Context<Self>) {
        self.extras.dismissed_hover_target = None;
        if !self.extras.hover_definition.is_empty() {
            self.extras.hover_definition.clear();
            cx.notify();
        }
        self.clear_language_hover(cx);
    }

    fn clear_language_hover(&mut self, cx: &mut Context<Self>) {
        if self
            .extras
            .hover_popover
            .take()
            .is_some_and(|popover| popover.visible())
        {
            cx.notify();
        }
        self.extras.lsp._hover_task = Task::ready(Ok(()));
    }
}
