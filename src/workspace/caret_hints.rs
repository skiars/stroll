use std::{ops::Range, sync::Arc, time::Duration};

use gpui::*;
use gpui_component::{
    ActiveTheme as _, ThemeStyled as _,
    input::{
        CaretAnchoredPopup, EditorState, RangeDecoration, RangeDecorationCollection,
        RangeDecorationStyle, RopeExt as _,
    },
};
use lsp_types::{
    DocumentHighlightKind, ParameterLabel, SignatureHelp, SignatureHelpContext,
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
    decorations: RangeDecorationCollection,
    snapshot: Option<Snapshot>,
    highlights: Task<()>,
    signature_request: Task<()>,
    signature: Option<SignatureHelp>,
    signature_active: bool,
}

impl CaretHints {
    pub(super) fn new(editor: &mut EditorState, cx: &mut Context<EditorState>) -> Self {
        Self {
            decorations: editor.create_range_decorations_collection(Vec::new(), cx),
            snapshot: None,
            highlights: Task::ready(()),
            signature_request: Task::ready(()),
            signature: None,
            signature_active: false,
        }
    }

    pub(super) fn clear(&mut self, cx: &mut App) {
        self.snapshot = None;
        self.highlights = Task::ready(());
        self.decorations.clear(cx);
        self.dismiss_signature();
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
        if moved {
            hints.highlights = Task::ready(());
            hints.decorations.clear(cx);
            let at_word = snapshot.caret < text.len() && text.word_range(snapshot.caret).is_some();
            if at_word {
                let expected = snapshot.clone();
                let text = text.clone();
                let document = document.clone();
                let path = path.clone();
                let editor = editor.downgrade();
                hints.highlights = cx.spawn_in(window, async move |workspace, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(120))
                        .await;
                    let request = document.document_highlights(text.clone(), expected.caret);
                    let Ok(highlights) = request.await else {
                        return;
                    };
                    _ = workspace.update_in(cx, |this, window, cx| {
                        let Some(file) = this
                            .current_caret_file(&path, &editor, &expected, &document, window, cx)
                        else {
                            return;
                        };
                        let foreground = cx.theme().foreground;
                        let highlights = highlights
                            .into_iter()
                            .filter_map(|highlight| {
                                let start = byte_offset(&text, highlight.range.start)?;
                                let end = byte_offset(&text, highlight.range.end)?;
                                (start < end).then(|| {
                                    let write =
                                        highlight.kind == Some(DocumentHighlightKind::WRITE);
                                    RangeDecoration::new(start..end)
                                        .with_style(if write {
                                            RangeDecorationStyle::Frame
                                        } else {
                                            RangeDecorationStyle::Fill
                                        })
                                        .with_color(foreground.opacity(if write {
                                            0.42
                                        } else {
                                            0.14
                                        }))
                                })
                            })
                            .collect();
                        file.caret_hints.decorations.clone().set(highlights, cx);
                    });
                });
            }
        }

        let Some(options) = document.signature_options() else {
            return;
        };
        let typed = previous.is_some_and(|previous| previous.revision != snapshot.revision);
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
