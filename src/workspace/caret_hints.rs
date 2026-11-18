use std::{collections::HashSet, ops::Range, sync::Arc, time::Duration};

use gpui::*;
use gpui_component::{
    ActiveTheme as _, ThemeStyled as _,
    input::{
        CaretAnchoredPopup, EditorState, RangeDecoration, RangeDecorationCollection,
        RangeDecorationStyle, Rope, RopeExt as _,
    },
};
use lsp_types::{
    DocumentHighlight, DocumentHighlightKind, ParameterLabel, SignatureHelp, SignatureHelpContext,
    SignatureHelpTriggerKind,
};

use super::{Workspace, WorkspaceView};
use crate::lsp::{Document, actions::byte_offset};

actions!(workspace, [ShowSignatureHelp]);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    revision: u64,
    caret: usize,
}

pub(super) struct CaretHints {
    decorations: [RangeDecorationCollection; 2],
    snapshot: Option<Snapshot>,
    highlights: Task<()>,
    highlighted_word: Option<String>,
    signature_request: Task<()>,
    signature: Option<SignatureHelp>,
    signature_active: bool,
}

impl CaretHints {
    pub(super) fn new(editor: &mut EditorState, cx: &mut Context<EditorState>) -> Self {
        Self {
            decorations: std::array::from_fn(|_| {
                editor.create_range_decorations_collection(Vec::new(), cx)
            }),
            snapshot: None,
            highlights: Task::ready(()),
            highlighted_word: None,
            signature_request: Task::ready(()),
            signature: None,
            signature_active: false,
        }
    }

    pub(super) fn clear(&mut self, cx: &mut App) {
        self.snapshot = None;
        self.highlights = Task::ready(());
        self.clear_highlights(cx);
        self.dismiss_signature();
    }

    fn clear_highlights(&mut self, cx: &mut App) {
        self.highlighted_word = None;
        for collection in &self.decorations {
            collection.clear(cx);
        }
    }

    pub(super) fn highlight_ranges(&self, cx: &App) -> Vec<Range<usize>> {
        self.decorations
            .iter()
            .flat_map(|collection| collection.get_ranges(cx))
            .collect()
    }

    fn retain_unchanged_highlights(&self, text: &Rope, cx: &mut App) {
        let Some(word) = &self.highlighted_word else {
            return;
        };
        for (kind, collection) in self.decorations.iter().enumerate() {
            let ranges = collection.get_ranges(cx);
            let unchanged = |range: &Range<usize>| {
                text.slice(range.clone()) == word.as_str()
                    && text.word_range(range.start).as_ref() == Some(range)
            };
            if !ranges.iter().all(unchanged) {
                let foreground = cx.theme().foreground;
                collection.set(
                    ranges
                        .into_iter()
                        .filter(unchanged)
                        .map(|range| symbol_decoration(range, kind, foreground))
                        .collect(),
                    cx,
                );
            }
        }
    }

    fn show_highlights(&mut self, word: Option<String>, highlights: SymbolRanges, cx: &mut App) {
        self.highlighted_word = word.filter(|_| !highlights.iter().all(Vec::is_empty));
        let foreground = cx.theme().foreground;
        for (kind, (collection, ranges)) in self.decorations.iter().zip(highlights).enumerate() {
            collection.set(
                ranges
                    .into_iter()
                    .map(|range| symbol_decoration(range, kind, foreground))
                    .collect(),
                cx,
            );
        }
    }

    fn dismiss_signature(&mut self) {
        self.signature_request = Task::ready(());
        self.signature = None;
        self.signature_active = false;
    }

    pub(super) fn popup(
        &self,
        editor: &Entity<EditorState>,
        window: &Window,
        cx: &App,
    ) -> Option<impl IntoElement> {
        let state = editor.read(cx);
        if !state.focus_handle(cx).is_focused(window) || state.is_composing() {
            return None;
        }
        let (label, parameter) = signature_label(self.signature.as_ref()?)?;
        let editor = editor.downgrade();
        Some(deferred(
            CaretAnchoredPopup::new(editor.clone(), move |_, max_height, size, window, cx| {
                let Some(editor) = editor.upgrade() else {
                    return div().into_any_element();
                };
                let state = editor.read(cx);
                if state.completion_menu_state().open || state.code_action_menu_state().open {
                    return div().into_any_element();
                }
                let foreground = cx.theme().foreground;
                let label = StyledText::new(label.clone()).with_highlights(parameter.clone().map(
                    |range| {
                        (
                            range,
                            HighlightStyle {
                                font_weight: Some(FontWeight::BOLD),
                                color: Some(cx.theme().primary),
                                ..Default::default()
                            },
                        )
                    },
                ));
                div()
                    .id("editor-signature-help")
                    .debug_selector(|| "editor-signature-help".into())
                    .popover_style(cx)
                    .max_w(
                        (window.viewport_size().width - px(16.))
                            .min(px(580.))
                            .max(px(0.)),
                    )
                    .max_h(max_height)
                    .overflow_hidden()
                    .px_3()
                    .py_2()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size((size - px(0.5)).max(px(1.)))
                    .text_color(foreground)
                    .child(label)
                    .into_any_element()
            })
            .prefer_above(),
        ))
    }
}

impl Workspace {
    pub(super) fn bind_caret_hint_keys(cx: &mut App) {
        cx.bind_keys([KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-shift-space"
            } else {
                "ctrl-shift-space"
            },
            ShowSignatureHelp,
            Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
        )]);
    }

    pub(super) fn show_signature_help(
        &mut self,
        _: &ShowSignatureHelp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.active_file().map(|file| file.editor.clone()) {
            self.refresh_caret_hints(&editor, true, window, cx);
        }
    }

    pub(super) fn dismiss_signature_help(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape"
            && let Some(path) = self.tabs.active()
            && let Some(file) = self.files.get_mut(path)
            && file.caret_hints.signature_active
            && !file.editor.read(cx).completion_menu_state().open
            && !file.editor.read(cx).code_action_menu_state().open
        {
            file.caret_hints.dismiss_signature();
            cx.notify();
            cx.stop_propagation();
        }
    }

    pub(super) fn refresh_caret_hints(
        &mut self,
        editor: &Entity<EditorState>,
        manual: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = editor.read(cx);
        let path = self.tabs.active();
        let available = self.view == WorkspaceView::Editor
            && !self.locator.open
            && !window.has_active_prompt()
            && !self.repository_overlay_open()
            && state.focus_handle(cx).is_focused(window)
            && !state.is_composing()
            && state.selected_range().is_empty();
        let Some(file) = self.files.values_mut().find(|file| file.editor == *editor) else {
            return;
        };
        let hints = &mut file.caret_hints;
        let Some(document) = file
            .language_server
            .as_ref()
            .filter(|_| available && path == Some(file.path.as_path()) && !file.missing)
            .map(|server| server.document.clone())
        else {
            if hints.snapshot.is_some() {
                hints.clear(cx);
                cx.notify();
            }
            return;
        };
        let state = editor.read(cx);
        let snapshot = Snapshot {
            revision: state.document_revision(),
            caret: state.cursor(),
        };
        let previous = hints.snapshot.replace(snapshot.clone());
        let moved = previous.as_ref() != Some(&snapshot);
        if !moved && !manual {
            return;
        }
        let text = state.text().clone();
        let path = file.path.clone();
        let typed = previous.is_some_and(|previous| previous.revision != snapshot.revision);
        if moved {
            hints.highlights = Task::ready(());
            let target = symbol_target(&text, snapshot.caret);
            if typed {
                hints.retain_unchanged_highlights(&text, cx);
            } else if !target.as_ref().is_some_and(|(range, _)| {
                hints.highlighted_word.as_deref().is_some_and(|word| {
                    text.slice(range.clone()) == word && hints.highlight_ranges(cx).contains(range)
                })
            }) {
                hints.clear_highlights(cx);
            }
            let displayed = hints.highlight_ranges(cx);
            if target.is_some() || !displayed.is_empty() {
                let expected = snapshot.clone();
                let text = text.clone();
                let document = document.clone();
                let path = path.clone();
                let editor = editor.downgrade();
                let retained_word = hints.highlighted_word.clone();
                hints.highlights = cx.spawn_in(window, async move |workspace, cx| {
                    let timer = cx.background_executor();
                    timer.timer(Duration::from_millis(120)).await;
                    // Give a result that would remove visible ranges a second
                    // chance after 270 ms of quiet. A newer target cancels both.
                    let settle = timer.timer(Duration::from_millis(150));
                    let word = target
                        .as_ref()
                        .map(|(range, _)| text.slice(range.clone()).to_string());
                    let Ok(mut highlights) =
                        request_symbol_ranges(&document, &text, target.as_ref()).await
                    else {
                        return;
                    };
                    let removes_highlights = || {
                        let ranges: HashSet<_> = highlights.iter().flatten().collect();
                        displayed.iter().any(|range| !ranges.contains(range))
                    };
                    if !displayed.is_empty() && (word != retained_word || removes_highlights()) {
                        settle.await;
                        let Ok(confirmed) =
                            request_symbol_ranges(&document, &text, target.as_ref()).await
                        else {
                            return;
                        };
                        highlights = confirmed;
                    }
                    _ = workspace.update_in(cx, |this, window, cx| {
                        if this
                            .current_caret_file(&path, &editor, &expected, &document, window, cx)
                            .is_some()
                        {
                            this.files
                                .get_mut(&path)
                                .unwrap()
                                .caret_hints
                                .show_highlights(word, highlights, cx);
                        }
                    });
                });
            }
        }

        let Some(options) = document.signature_options() else {
            return;
        };
        let preceding = typed
            .then(|| text.chars_at(snapshot.caret).reversed().next())
            .flatten();
        let trigger = preceding.filter(|ch| {
            let triggers = options.trigger_characters.as_ref().into_iter().flatten();
            let retriggers = options.retrigger_characters.as_ref().into_iter().flatten();
            triggers
                .chain(retriggers.filter(|_| hints.signature_active))
                .any(|trigger| trigger.chars().eq(std::iter::once(*ch)))
        });
        if !manual && trigger.is_none() && !hints.signature_active {
            return;
        }
        let context = SignatureHelpContext {
            trigger_kind: if manual {
                SignatureHelpTriggerKind::INVOKED
            } else if trigger.is_some() {
                SignatureHelpTriggerKind::TRIGGER_CHARACTER
            } else {
                SignatureHelpTriggerKind::CONTENT_CHANGE
            },
            trigger_character: trigger.filter(|_| !manual).map(|ch| ch.to_string()),
            is_retrigger: hints.signature_active,
            active_signature_help: hints.signature.clone(),
        };
        if moved && !typed {
            hints.signature = None;
            cx.notify();
        }
        hints.signature_active = true;
        let editor = editor.downgrade();
        hints.signature_request = cx.spawn_in(window, async move |workspace, cx| {
            if !manual && trigger.is_none() {
                cx.background_executor()
                    .timer(Duration::from_millis(45))
                    .await;
            }
            let result = document.signature_help(text, snapshot.caret, context).await;
            _ = workspace.update_in(cx, |this, window, cx| {
                if this
                    .current_caret_file(&path, &editor, &snapshot, &document, window, cx)
                    .is_none()
                {
                    return;
                }
                let hints = &mut this.files.get_mut(&path).unwrap().caret_hints;
                hints.signature = result
                    .ok()
                    .flatten()
                    .filter(|help| !help.signatures.is_empty());
                hints.signature_active = hints.signature.is_some();
                cx.notify();
            });
        });
    }

    fn current_caret_file(
        &self,
        path: &std::path::Path,
        editor: &WeakEntity<EditorState>,
        snapshot: &Snapshot,
        document: &Arc<Document>,
        window: &Window,
        cx: &App,
    ) -> Option<&super::OpenFile> {
        let file = self.active_file().filter(|file| {
            self.view == WorkspaceView::Editor
                && !self.locator.open
                && !self.repository_overlay_open()
                && file.path == path
                && !file.missing
                && editor.upgrade().as_ref() == Some(&file.editor)
                && file.caret_hints.snapshot.as_ref() == Some(snapshot)
                && file
                    .language_server
                    .as_ref()
                    .is_some_and(|server| Arc::ptr_eq(&server.document, document))
        })?;
        let state = file.editor.read(cx);
        (state.document_revision() == snapshot.revision
            && state.focus_handle(cx).is_focused(window)
            && state.cursor() == snapshot.caret
            && state.selected_range().is_empty()
            && !state.is_composing())
        .then_some(file)
    }
}

type SymbolRanges = [Vec<Range<usize>>; 2];

fn symbol_target(text: &Rope, caret: usize) -> Option<(Range<usize>, usize)> {
    let previous_character = || {
        text.chars_at(caret)
            .reversed()
            .next()
            .map(|ch| caret - ch.len_utf8())
    };
    let range = text.word_range(caret).or_else(|| {
        (caret == text.len())
            .then(|| previous_character().and_then(|offset| text.word_range(offset)))
            .flatten()
            .filter(|range| range.end == caret)
    })?;
    // Some servers return no highlights for the boundary immediately after a word.
    let position = if caret == range.end {
        previous_character()?
    } else {
        caret
    };
    Some((range, position))
}

async fn request_symbol_ranges(
    document: &Document,
    text: &Rope,
    target: Option<&(Range<usize>, usize)>,
) -> anyhow::Result<SymbolRanges> {
    let mut ranges = SymbolRanges::default();
    if let Some((_, position)) = target {
        let highlights = document
            .document_highlights(text.clone(), *position)
            .await?;
        for DocumentHighlight { range, kind } in highlights {
            if let (Some(start), Some(end)) =
                (byte_offset(text, range.start), byte_offset(text, range.end))
                && start < end
            {
                ranges[usize::from(kind == Some(DocumentHighlightKind::WRITE))].push(start..end);
            }
        }
    }
    Ok(ranges)
}

fn symbol_decoration(range: Range<usize>, kind: usize, foreground: Hsla) -> RangeDecoration {
    let (style, opacity) = if kind == 1 {
        (RangeDecorationStyle::Frame, 0.42)
    } else {
        (RangeDecorationStyle::Fill, 0.14)
    };
    RangeDecoration::new(range)
        .with_style(style)
        .with_color(foreground.opacity(opacity))
}

fn signature_label(help: &SignatureHelp) -> Option<(SharedString, Option<Range<usize>>)> {
    let signature = help
        .signatures
        .get(help.active_signature.unwrap_or(0) as usize)
        .or_else(|| help.signatures.first())?;
    let active = signature
        .active_parameter
        .or(help.active_parameter)
        .unwrap_or(0);
    let parameter = signature
        .parameters
        .as_ref()
        .and_then(|parameters| parameters.get(active as usize))
        .and_then(|parameter| match &parameter.label {
            ParameterLabel::Simple(label) => signature
                .label
                .find(label)
                .map(|start| start..start + label.len()),
            ParameterLabel::LabelOffsets([start, end]) => {
                let start = utf16_boundary(&signature.label, *start)?;
                let end = utf16_boundary(&signature.label, *end)?;
                (start < end).then_some(start..end)
            }
        });
    Some((signature.label.clone().into(), parameter))
}

fn utf16_boundary(label: &str, offset: u32) -> Option<usize> {
    let mut remaining = offset;
    for (index, ch) in label.char_indices() {
        if remaining == 0 {
            return Some(index);
        }
        remaining = remaining.checked_sub(ch.len_utf16() as u32)?;
    }
    (remaining == 0).then_some(label.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn signature_selects_the_active_overload_and_valid_utf16_parameter() {
        let help: SignatureHelp = serde_json::from_value(serde_json::json!({
            "signatures": [
                {"label": "f()"},
                {
                    "label": "f(😀, second)",
                    "activeParameter": 1,
                    "parameters": [{"label": [2, 4]}, {"label": [6, 12]}]
                }
            ],
            "activeSignature": 1,
            "activeParameter": 0
        }))
        .unwrap();
        let (label, parameter) = signature_label(&help).unwrap();
        assert_eq!(&label[parameter.unwrap()], "second");
        assert_eq!(utf16_boundary(&label, 3), None);
    }
}
