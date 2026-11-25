use std::collections::HashSet;

use gpui_component::{
    highlighter::LanguageRegistry,
    input::{Rope, RopeExt},
};
use lsp_types::{
    CodeAction, CodeActionKind, Diagnostic, DocumentChanges, NumberOrString,
    OptionalVersionedTextDocumentIdentifier, Range, TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};

use super::{OpenDocument, actions::byte_offset, lsp_position};

const NOLINT: &str = "NOLINT";

pub(super) fn suppression_actions(
    uri: &str,
    document: &OpenDocument,
    diagnostics: &[&Diagnostic],
) -> Vec<CodeAction> {
    let mut seen = HashSet::new();
    let candidates: Vec<_> = diagnostics
        .iter()
        .copied()
        .filter_map(|diagnostic| {
            let check = check_name(diagnostic)?;
            let position = diagnostic.range.start;
            byte_offset(&document.text, position)?;
            seen.insert((position.line, check))
                .then_some((diagnostic, check))
        })
        .take(100)
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let Some((uri, syntax)) = uri
        .parse::<Uri>()
        .ok()
        .zip(syntax_tree(&document.text, &document.language))
    else {
        return Vec::new();
    };
    candidates
        .into_iter()
        .filter_map(|(diagnostic, check)| {
            let edit =
                suppression_edit(&document.text, &syntax, diagnostic.range.start.line, check)?;
            Some(CodeAction {
                title: format!("Suppress {check} on this line"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(WorkspaceEdit {
                    document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                        text_document: OptionalVersionedTextDocumentIdentifier::new(
                            uri.clone(),
                            document.version,
                        ),
                        edits: vec![lsp_types::OneOf::Left(edit)],
                    }])),
                    ..Default::default()
                }),
                ..Default::default()
            })
        })
        .collect()
}

fn check_name(diagnostic: &Diagnostic) -> Option<&str> {
    let Some(NumberOrString::String(check)) = &diagnostic.code else {
        return None;
    };
    (diagnostic.source.as_deref() == Some("clang-tidy")
        && check != "clang-tidy-nolint"
        && check.starts_with(|c: char| c.is_ascii_alphabetic())
        && check
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    .then_some(check.as_str())
}

fn syntax_tree(text: &Rope, language: &str) -> Option<tree_sitter::Tree> {
    let language = match language {
        "c" | "objective-c" => "c",
        _ => "cpp",
    };
    let grammar = LanguageRegistry::singleton().language(language)?.language?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar).ok()?;
    parser.parse(text.to_string(), None)
}

fn suppression_edit(
    text: &Rope,
    syntax: &tree_sitter::Tree,
    row: u32,
    check: &str,
) -> Option<TextEdit> {
    let line_start = text.line_start_offset(row as usize);
    let line = text.slice_line(row as usize).to_string();
    let line = line.trim_end_matches([' ', '\t', '\r']);
    if line.is_empty() || continues_on_next_line(line) {
        return None;
    }
    if row > 0 && continues_on_next_line(&text.slice_line(row as usize - 1).to_string()) {
        return None;
    }

    let end = line_start + line.len();
    let mut node = syntax.root_node().descendant_for_byte_range(end - 1, end);
    while let Some(current) = node {
        if current.kind() == "comment" {
            let start = current.start_byte();
            let comment = text.slice(start..end).to_string();
            if start >= line_start && (comment.starts_with("//") || current.end_byte() == end) {
                for (index, _) in comment.match_indices(NOLINT) {
                    let suffix = &comment[index + NOLINT.len()..];
                    if suffix.starts_with(|c: char| c.is_ascii_alphabetic()) {
                        continue;
                    }
                    let (checks, _) = suffix.strip_prefix('(')?.split_once(')')?;
                    if checks.split(',').any(|existing| existing.trim() == check) {
                        return None;
                    }
                    let offset = start + index + NOLINT.len() + 1 + checks.len();
                    let separator = if checks.trim().is_empty() { "" } else { ", " };
                    return Some(insert(text, offset, format!("{separator}{check}")));
                }
                if comment.starts_with("//") {
                    return Some(insert(text, end, format!(" {NOLINT}({check})")));
                }
            }
        }
        // A suffix inside a multiline token would become part of that token.
        if current.end_byte() > end
            && matches!(
                current.kind(),
                "comment" | "raw_string_literal" | "string_literal" | "char_literal"
            )
        {
            return None;
        }
        node = current.parent();
    }

    Some(insert(text, end, format!("  // {NOLINT}({check})")))
}

fn continues_on_next_line(line: &str) -> bool {
    let line = line.trim_end();
    line.ends_with('\\') || line.ends_with("??/")
}

fn insert(text: &Rope, offset: usize, replacement: String) -> TextEdit {
    let position = lsp_position(text, offset);
    TextEdit::new(Range::new(position, position), replacement)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extends_an_existing_inline_directive() {
        let text = Rope::from("  int n;  // NOLINT(readability-other): explanation\r\n");
        let syntax = syntax_tree(&text, "cpp").unwrap();
        let edit = suppression_edit(&text, &syntax, 0, "bugprone-example").unwrap();
        let mut result = text.clone();
        result.insert(
            byte_offset(&text, edit.range.start).unwrap(),
            &edit.new_text,
        );
        assert_eq!(
            result,
            "  int n;  // NOLINT(readability-other, bugprone-example): explanation\r\n"
        );
        assert!(suppression_edit(&text, &syntax, 0, "readability-other").is_none());
    }

    #[test]
    fn appends_to_a_line_comment_and_preserves_next_line_directives() {
        let text = Rope::from("int n; // NOLINTNEXTLINE(readability-other)  \r\nint m;\r\n");
        let syntax = syntax_tree(&text, "cpp").unwrap();
        let edit = suppression_edit(&text, &syntax, 0, "bugprone-example").unwrap();
        let mut result = text.clone();
        result.insert(
            byte_offset(&text, edit.range.start).unwrap(),
            &edit.new_text,
        );
        assert_eq!(
            result,
            "int n; // NOLINTNEXTLINE(readability-other) NOLINT(bugprone-example)  \r\nint m;\r\n"
        );
    }

    #[test]
    fn skips_insertions_that_would_change_a_multiline_token_or_spliced_line() {
        for source in [
            "const char *s = R\"(text\nraw string content\n)\";\n",
            "/* text\n comment\n */ int n;\n",
            "#define VALUE \\\n    42\n",
        ] {
            let text = Rope::from(source);
            let syntax = syntax_tree(&text, "cpp").unwrap();
            assert!(
                suppression_edit(&text, &syntax, 1, "bugprone-example").is_none(),
                "{source}"
            );
        }
    }
}
