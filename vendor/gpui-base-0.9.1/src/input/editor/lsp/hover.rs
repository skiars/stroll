use anyhow::Result;
use gpui::{App, Bounds, Context, MouseMoveEvent, Pixels, Point, SharedString, Task, Window};
use instant::Duration;
use ropey::Rope;
use std::{collections::BTreeMap, rc::Rc, sync::Arc};

use crate::input::{DiagnosticSeverity, EditorMode, HoverPopoverState, InputBaseState, RopeExt};

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

impl InputBaseState<EditorMode> {
    /// Handle hover trigger LSP request.
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

        if let Some(popover) = self.extras.hover_popover.as_ref()
            && (popover.target_range.contains(&offset) || popover.target_range.start == offset)
            && diagnostics.iter().copied().eq(popover.diagnostics.iter())
        {
            if let Some(pointer) = self
                .extras
                .hover_popover
                .as_mut()
                .and_then(|p| p.pointer.as_mut())
            {
                *pointer = position;
            }
            return;
        }

        let provider = self.extras.lsp.hover_provider.clone();
        if diagnostics.is_empty() && provider.is_none() {
            self.clear_language_hover(cx);
            return;
        }
        let should_delay = self.hover_popover().is_none();
        let word = self.text.word_range(offset).unwrap_or(offset..offset);
        let target_range = diagnostics
            .first()
            .map_or_else(|| word.clone(), |entry| entry.range.clone());
        // A multi-line diagnostic can begin or end outside the viewport.
        // Anchor to the hovered word if the full range cannot fit on this row.
        let symbol_range = if diagnostics.is_empty()
            || self.range_to_bounds(&target_range).is_some_and(|bounds| {
                self.range_to_bounds(&(offset..offset))
                    .is_some_and(|caret| bounds.size.height == caret.size.height)
            }) {
            target_range.clone()
        } else {
            word
        };
        let diagnostics: Rc<[_]> = diagnostics.into_iter().cloned().collect();
        self.extras.lsp._hover_task = Task::ready(Ok(()));
        let popover = HoverPopoverState {
            symbol_range,
            target_range,
            pointer: Some(position),
            diagnostics,
            hover: None,
            documentation: None,
        };
        if popover.visible() || !should_delay {
            cx.notify();
        }
        self.extras.hover_popover = Some(popover);

        let Some(provider) = provider else {
            return;
        };
        let text = self.text.clone();
        let revision = self.document_revision;
        // Ask the provider only after the initial delay. Movement within the
        // same hover target keeps the request and the visible diagnostic rows.
        self.extras.lsp._hover_task = cx.spawn_in(window, async move |this, cx| {
            if should_delay {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
            }

            let task = this.update_in(cx, |_, window, cx| {
                provider.hover(&text, offset, window, cx)
            })?;
            let Some(hover) = task.await? else {
                return Ok(());
            };

            let enrichment = this.update_in(cx, |editor, window, cx| {
                if editor.document_revision != revision {
                    return None;
                }
                let popover = editor.extras.hover_popover.as_mut()?;
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
                popover.hover = Some(hover.clone());
                cx.notify();
                Some(enrichment)
            })?;
            if let Some(enrichment) = enrichment
                && let Some(documentation) = enrichment.await
            {
                this.update_in(cx, |editor, _, cx| {
                    if editor.document_revision == revision
                        && let Some(popover) = editor.extras.hover_popover.as_mut()
                        && popover.hover.as_ref() == Some(&hover)
                    {
                        popover.documentation = Some(documentation);
                        cx.notify();
                    }
                })?;
            }
            Ok(())
        });
    }

    pub(crate) fn handle_mouse_move(
        &mut self,
        offset: usize,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.alt {
            self.clear_hover_state(cx);
            return;
        }
        if event.modifiers.secondary(cx) {
            self.clear_language_hover(cx);
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
                self.clear_language_hover(cx);
            }
        }
    }

    pub(crate) fn validate_language_hover(&mut self, clip: Bounds<Pixels>, cx: &mut Context<Self>) {
        let Some(popover) = self.extras.hover_popover.as_ref() else {
            return;
        };
        let Some(pointer) = popover.pointer else {
            return;
        };
        let valid = self.input_bounds.contains(&pointer)
            && clip.contains(&pointer)
            && self.text_offset_at_point(pointer).is_some_and(|offset| {
                popover.target_range.contains(&offset) || popover.target_range.start == offset
            })
            && self.range_to_bounds(&popover.symbol_range).is_some();
        if !valid {
            self.clear_language_hover(cx);
        }
    }

    pub fn clear_hover_state(&mut self, cx: &mut Context<Self>) {
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
