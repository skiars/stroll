use std::ops::Range;

use anyhow::{Context as _, Result};
use gpui_component::input::{EditorState, InputEditsEvent, Rope, RopeExt};
use lsp_types::TextEdit;

use super::{Document, Query, actions::validated_text_edits, lsp_position};

#[derive(Clone, Copy)]
pub(crate) enum NewlineServer {
    OnType,
    RustAnalyzer,
}

/// The first new line of a single Enter edit. A bracket split can insert
/// another line for the closing bracket.
#[derive(Clone, Copy)]
pub(crate) struct Newline {
    start: usize,
    end: usize,
    caret: usize,
    indent_end: usize,
    revision: u64,
}

impl Newline {
    pub(crate) fn capture(event: &InputEditsEvent, editor: &EditorState) -> Option<Self> {
        let [(edit, after)] = event.edits() else {
            return None;
        };
        if edit.start_byte != edit.old_end_byte || after != editor.text() {
            return None;
        }
        let start = edit.start_byte;
        let end = edit.new_end_byte;
        let caret = editor.cursor();
        if caret <= start || caret > end || after.byte(start) != b'\n' {
            return None;
        }
        let first_line = after.slice(start + 1..caret);
        let insertion = after.slice(caret..end);
        if !first_line.bytes().all(horizontal_space)
            || !insertion
                .bytes()
                .all(|byte| byte == b'\n' || horizontal_space(byte))
        {
            return None;
        }
        let indent_end = caret
            + after
                .slice(caret..)
                .bytes()
                .take_while(|&byte| horizontal_space(byte))
                .count();
        Some(Self {
            start,
            end,
            caret,
            indent_end,
            revision: editor.document_revision(),
        })
    }

    pub(crate) fn is_current(self, editor: &EditorState) -> bool {
        editor.document_revision() == self.revision
            && editor.cursor() == self.caret
            && editor.selected_range().is_empty()
    }

    pub(crate) fn indentation(self) -> Range<usize> {
        self.start + 1..self.indent_end
    }
}

impl Document {
    pub(crate) fn newline_indentation(
        &self,
        text: Rope,
        newline: Newline,
        server: NewlineServer,
    ) -> impl Future<Output = Result<Option<String>>> + use<> {
        let (query, source) = match server {
            NewlineServer::OnType => (
                Query::IndentOnType(lsp_position(&text, newline.caret)),
                text.clone(),
            ),
            NewlineServer::RustAnalyzer => {
                let mut before = text.clone();
                before.replace(newline.start..newline.end, "");
                let position = lsp_position(&before, newline.start);
                (Query::RustOnEnter(position, text.clone()), before)
            }
        };
        // Queue the snapshot before the editor can send a newer change.
        let request = self.begin_request(query, source);
        async move {
            let response = request?.await.context("The LSP session is closed")??;
            let edits: Option<Vec<TextEdit>> = serde_json::from_value(response.value)?;
            match (edits, server) {
                (Some(edits), NewlineServer::OnType) => {
                    on_type_indentation(text, newline.indent_end, edits)
                }
                (Some(edits), NewlineServer::RustAnalyzer) => {
                    Ok(rust_on_enter_indentation(&text, newline.start, &edits))
                }
                (None, _) => Ok(None),
            }
        }
    }
}

fn horizontal_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

fn on_type_indentation(
    mut text: Rope,
    indent_end: usize,
    edits: Vec<TextEdit>,
) -> Result<Option<String>> {
    let edits = validated_text_edits(&text, edits)?;
    let mut mapped = indent_end;
    for (range, replacement) in edits.iter().rev() {
        // A replacement that swallows the end of the indent, or an insertion
        // of another line there, makes the target line ambiguous.
        if (range.start < indent_end && indent_end < range.end)
            || (range.is_empty() && range.start == indent_end && replacement.contains('\n'))
        {
            return Ok(None);
        }
        if range.end <= indent_end {
            mapped = mapped - range.len() + replacement.len();
        }
        text.replace(range.clone(), replacement);
    }
    let point = text.offset_to_point(mapped);
    let line = text.slice_line(point.row);
    let width = line
        .bytes()
        .take_while(|&byte| horizontal_space(byte))
        .count();
    // Only use a result that maps the indent boundary to leading space on
    // the new line, even if clangd also returned other text edits.
    Ok((point.row > 0 && point.column <= width).then(|| line.slice(..width).to_string()))
}

fn rust_on_enter_indentation(text: &Rope, at: usize, edits: &[TextEdit]) -> Option<String> {
    let [edit] = edits else {
        return None;
    };
    if edit.range.start != lsp_position(text, at) {
        return None;
    }
    // rust-analyzer puts the final caret at $0. Consume only horizontal
    // whitespace directly before that marker on the first inserted line.
    let after_newline = edit.new_text.strip_prefix('\n')?;
    let (indentation, _) = after_newline.split_once("$0")?;
    indentation
        .bytes()
        .all(horizontal_space)
        .then(|| indentation.to_owned())
}

#[cfg(test)]
mod tests {
    use lsp_types::{Position, Range as LspRange};

    use super::*;

    #[test]
    fn clangd_indent_tracks_the_new_line_through_edits_to_earlier_lines() -> Result<()> {
        let text = Rope::from("void f() { int a;\n  int b; }\n");
        let anchor = text.to_string().find("int b").unwrap();
        let edits = vec![
            TextEdit::new(
                LspRange::new(Position::new(0, 10), Position::new(0, 11)),
                "\n    ".into(),
            ),
            TextEdit::new(
                LspRange::new(Position::new(0, 17), Position::new(1, 2)),
                "\n    ".into(),
            ),
        ];
        assert_eq!(
            on_type_indentation(text, anchor, edits)?.as_deref(),
            Some("    "),
        );
        Ok(())
    }

    #[test]
    fn rust_on_enter_accepts_only_indent_before_the_final_tab_stop() {
        let text = Rope::from("fn f() {}\n");
        let caret = 8;
        for (replacement, expected) in [
            ("\n    $0\n", Some("    ")),
            ("\n    // $0", None),
            ("\n    ${1:code}\n", None),
        ] {
            let at = lsp_position(&text, caret);
            let edit = TextEdit::new(LspRange::new(at, at), replacement.into());
            assert_eq!(
                rust_on_enter_indentation(&text, caret, &[edit]).as_deref(),
                expected,
            );
        }
    }
}
