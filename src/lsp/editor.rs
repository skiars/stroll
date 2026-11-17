use std::{ops::Range as ByteRange, path::PathBuf, rc::Rc, sync::Arc};

use anyhow::Result;
use gpui::{App, AppContext, Task, Window};
use gpui_component::input::{
    DefinitionProvider, DocumentRangeSemanticTokensProvider, HoverDocumentation, HoverProvider,
    InlayHint, Rope, RopeExt,
};
use lsp_types::{
    Hover, HoverContents, LocationLink, MarkedString, MarkupContent, MarkupKind, Position, Range,
    SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensLegend,
};

use crate::{ai::Service, repository::Repository, symbols::LocalSymbols};

use super::Document;

// The component captures its legend before starting the asynchronous request.
// Return stable theme names and translate the server's legend with its response.
pub(crate) const SEMANTIC_ROLES: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "constructor",
    "enum",
    "function",
    "keyword",
    "label",
    "number",
    "operator",
    "property",
    "punctuation",
    "string",
    "string.regex",
    "type",
    "variable",
    "variable.special",
    "variant",
    "namespace",
    "type.builtin",
    "type.interface",
    "type.parameter",
    "function.method",
    "function.macro",
    "keyword.modifier",
    "keyword.self",
];

pub(crate) const DECLARATION: u32 = 1;
pub(crate) const READONLY: u32 = 2;
pub(crate) const DEPRECATED: u32 = 4;
pub(crate) const SEMANTIC_VARIANTS: u32 = 8;

pub(crate) fn semantic_style_key(role: &str, flags: u32) -> String {
    format!("{role}.lsp.{flags}")
}

pub(super) const TOKEN_TYPES: &[&str] = &[
    "namespace",
    "type",
    "class",
    "enum",
    "interface",
    "struct",
    "typeParameter",
    "parameter",
    "variable",
    "property",
    "enumMember",
    "event",
    "function",
    "method",
    "member",
    "macro",
    "keyword",
    "modifier",
    "comment",
    "string",
    "number",
    "regexp",
    "operator",
    "decorator",
    "const",
    "static",
    "constParameter",
    "lifetime",
    "typeAlias",
    "union",
    "builtinType",
    "boolean",
    "character",
    "selfKeyword",
    "selfTypeKeyword",
    "attribute",
    "builtinAttribute",
    "derive",
    "deriveHelper",
    "procMacro",
    "label",
    "toolModule",
    "punctuation",
    "concept",
    "bracket",
    "brace",
    "parenthesis",
    "angle",
    "comma",
    "colon",
    "semicolon",
    "dot",
    "arithmetic",
    "bitwise",
    "comparison",
    "logical",
];

pub(super) const TOKEN_MODIFIERS: &[&str] = &[
    "declaration",
    "definition",
    "readonly",
    "static",
    "deprecated",
    "abstract",
    "async",
    "modification",
    "documentation",
    "defaultLibrary",
    "local",
    "constant",
    "mutable",
];

pub(crate) struct EditorLsp {
    pub document: Arc<Document>,
    pub fallback: LocalSymbols,
    pub(super) completion: super::completion::CompletionCache,
    pub(super) completion_activation: super::CompletionActivation,
    hover_context: (Service, Arc<Repository>, PathBuf),
}

impl EditorLsp {
    pub fn new(
        document: Document,
        language: &'static str,
        completion_activation: super::CompletionActivation,
        hover_context: (Service, Arc<Repository>, PathBuf),
    ) -> Rc<Self> {
        Rc::new(Self {
            document: Arc::new(document),
            fallback: LocalSymbols(language),
            completion: Default::default(),
            completion_activation,
            hover_context,
        })
    }
}

pub(crate) fn editor_inlay_hints(
    document: &Document,
    text: &Rope,
    ranges: &[ByteRange<usize>],
    cx: &App,
) -> Task<Result<Vec<InlayHint>>> {
    let requests = ranges
        .iter()
        .map(|rows| {
            let offsets = text.line_start_offset(rows.start)..text.line_start_offset(rows.end);
            (rows.clone(), document.inlay_hints(text, offsets))
        })
        .collect::<Vec<_>>();
    let text = text.clone();
    cx.background_spawn(async move {
        let mut hints = Vec::new();
        for (rows, request) in requests {
            hints.extend(request.await?.into_iter().filter_map(|hint| {
                rows.contains(&(hint.position.line as usize))
                    .then(|| editor_inlay_hint(&text, hint))
                    .flatten()
            }));
        }
        Ok(hints)
    })
}

fn editor_inlay_hint(text: &Rope, hint: lsp_types::InlayHint) -> Option<InlayHint> {
    let position = editor_position(text, hint.position)?;
    let offset = text.position_to_offset(&position);
    let row = position.line as usize;
    if offset > text.line_end_offset(row) || text.offset_to_point(offset).row != row {
        return None;
    }
    let mut label = String::new();
    if hint.padding_left == Some(true) {
        label.push(' ');
    }
    match hint.label {
        lsp_types::InlayHintLabel::String(value) => label.push_str(&value),
        lsp_types::InlayHintLabel::LabelParts(parts) => {
            for part in parts {
                label.push_str(&part.value);
            }
        }
    }
    if hint.padding_right == Some(true) {
        label.push(' ');
    }
    Some(InlayHint {
        offset,
        label: label.into(),
    })
}

impl HoverProvider for EditorLsp {
    fn code_languages(&self) -> Option<gpui_component::text::CodeLanguages> {
        self.hover_context.1.language_resources().code_languages()
    }

    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Hover>>> {
        if offset >= text.len()
            || !text.is_char_boundary(offset)
            || text.char(offset).is_whitespace()
        {
            return Task::ready(Ok(None));
        }
        let document = self.document.clone();
        let text = text.clone();
        let fallback = self.fallback;
        cx.background_spawn(async move {
            if let Ok(Some(mut hover)) = document.hover(text.clone(), offset).await {
                let (kind, value) = match hover.contents {
                    HoverContents::Markup(markup) => (markup.kind, markup.value),
                    HoverContents::Scalar(marked) => {
                        (MarkupKind::Markdown, marked_markdown(marked))
                    }
                    HoverContents::Array(parts) => (
                        MarkupKind::Markdown,
                        parts
                            .into_iter()
                            .map(marked_markdown)
                            .collect::<Vec<_>>()
                            .join("\n\n"),
                    ),
                };
                if !value.trim().is_empty() {
                    hover.contents = HoverContents::Markup(MarkupContent { kind, value });
                    hover.range = hover.range.and_then(|range| editor_range(&text, range));
                    return Ok(Some(hover));
                }
            }
            fallback.lookup_hover(&text, offset)
        })
    }

    fn enrich_hover(
        &self,
        hover: &Hover,
        _: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Option<HoverDocumentation>> {
        let (ai, repo, path) = &self.hover_context;
        let Some(request) = ai.snapshot(true) else {
            return Task::ready(None);
        };
        cx.background_spawn(request.translate_hover(
            repo.clone(),
            path.clone(),
            hover.contents.clone(),
        ))
    }
}

impl DefinitionProvider for EditorLsp {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<LocationLink>>> {
        if text.word_range(offset).is_none() {
            return Task::ready(Ok(Vec::new()));
        }
        let request = self.document.editor_definitions(text.clone(), offset, None);
        cx.background_spawn(async move { Ok(request.await) })
    }
}

impl Document {
    pub(crate) fn editor_definitions(
        &self,
        text: Rope,
        offset: usize,
        version: Option<i32>,
    ) -> impl Future<Output = Vec<LocationLink>> + use<> {
        let request = self.definitions_at_version(text.clone(), offset, version);
        async move {
            let mut links = request.await.unwrap_or_default();
            for link in &mut links {
                link.origin_selection_range = link
                    .origin_selection_range
                    .and_then(|range| editor_range(&text, range));
            }
            links
        }
    }
}

impl DocumentRangeSemanticTokensProvider for EditorLsp {
    fn legend(&self) -> SemanticTokensLegend {
        SemanticTokensLegend {
            token_types: SEMANTIC_ROLES
                .iter()
                .flat_map(|role| {
                    (0..SEMANTIC_VARIANTS)
                        .map(|flags| SemanticTokenType::from(semantic_style_key(role, flags)))
                })
                .collect(),
            token_modifiers: Vec::new(),
        }
    }

    fn semantic_tokens(
        &self,
        text: &Rope,
        _: ByteRange<usize>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<SemanticTokens>> {
        let text = text.clone();
        let request = self.document.semantic_tokens(text.clone());
        cx.background_spawn(async move {
            Ok(match request.await? {
                Some((tokens, legend)) => translate_tokens(&text, tokens, &legend),
                None => SemanticTokens::default(),
            })
        })
    }
}

fn translate_tokens(
    text: &Rope,
    tokens: SemanticTokens,
    legend: &SemanticTokensLegend,
) -> SemanticTokens {
    let mut data = Vec::with_capacity(tokens.data.len());
    let mut position = Position::default();
    let mut previous = Position::default();
    for token in tokens.data {
        let Some(line) = position.line.checked_add(token.delta_line) else {
            break;
        };
        let character = if token.delta_line > 0 {
            token.delta_start
        } else if let Some(character) = position.character.checked_add(token.delta_start) {
            character
        } else {
            break;
        };
        position = Position::new(line, character);
        let Some(end) = character.checked_add(token.length) else {
            continue;
        };
        let Some(style) = theme_style(token, legend) else {
            continue;
        };
        let Some(range) = editor_range(text, Range::new(position, Position::new(line, end))) else {
            continue;
        };
        data.push(SemanticToken {
            delta_line: line - previous.line,
            delta_start: if line == previous.line {
                range.start.character - previous.character
            } else {
                range.start.character
            },
            length: range.end.character - range.start.character,
            token_type: style,
            token_modifiers_bitset: 0,
        });
        previous = range.start;
    }
    SemanticTokens {
        result_id: None,
        data,
    }
}

fn theme_style(token: SemanticToken, legend: &SemanticTokensLegend) -> Option<u32> {
    let name = legend.token_types.get(token.token_type as usize)?.as_str();
    let modifier = |name| {
        legend
            .token_modifiers
            .iter()
            .take(32)
            .enumerate()
            .any(|(index, item)| {
                item.as_str() == name && token.token_modifiers_bitset & (1 << index) != 0
            })
    };
    let name = match name {
        "class" | "struct" | "typeAlias" | "union" | "concept" => "type",
        "interface" => "type.interface",
        "builtinType" => "type.builtin",
        "typeParameter" | "lifetime" => "type.parameter",
        "parameter" => "variable.special",
        "variable" if modifier("constant") || (modifier("readonly") && !modifier("local")) => {
            "constant"
        }
        "property" | "field" | "event" => "property",
        "enumMember" => "variant",
        "method" | "member" => "function.method",
        "macro" | "procMacro" => "function.macro",
        "modifier" => "keyword.modifier",
        "selfKeyword" | "selfTypeKeyword" => "keyword.self",
        "comment" if modifier("documentation") => "comment.doc",
        "character" | "char" => "string",
        "regexp" => "string.regex",
        "decorator" | "builtinAttribute" | "derive" | "deriveHelper" | "toolModule" => "attribute",
        "const" | "constParameter" => "constant",
        "static" => "variable",
        "bracket" | "brace" | "parenthesis" | "angle" | "comma" | "colon" | "semicolon" | "dot" => {
            "punctuation"
        }
        "arithmetic" | "bitwise" | "comparison" | "logical" => "operator",
        name => name,
    };
    let flags = [
        (
            DECLARATION,
            modifier("declaration") || modifier("definition"),
        ),
        (READONLY, modifier("readonly")),
        (DEPRECATED, modifier("deprecated")),
    ]
    .into_iter()
    .filter_map(|(flag, set)| set.then_some(flag))
    .fold(0, |flags, flag| flags | flag);

    SEMANTIC_ROLES
        .iter()
        .position(|&style| style == name)
        .map(|index| index as u32 * SEMANTIC_VARIANTS + flags)
}

// LSP uses UTF-16 columns. GPUI's editor positions count Unicode scalar values.
pub(crate) fn editor_position(text: &Rope, position: Position) -> Option<Position> {
    let row = position.line as usize;
    if row >= text.lines_len() {
        return None;
    }
    let line = text.slice_line(row);
    let column = position.character as usize;
    if column > line.len_utf16() {
        return None;
    }
    let byte = line.utf16_to_byte_idx(column);
    if line.byte_to_utf16_idx(byte) != column {
        return None;
    }
    Some(Position::new(
        position.line,
        line.byte_to_char_idx(byte) as u32,
    ))
}

fn editor_range(text: &Rope, range: Range) -> Option<Range> {
    let start = editor_position(text, range.start)?;
    let end = editor_position(text, range.end)?;
    (start < end).then_some(Range::new(start, end))
}

pub(crate) fn editor_diagnostics(
    text: &Rope,
    diagnostics: Vec<lsp_types::Diagnostic>,
) -> Vec<lsp_types::Diagnostic> {
    let mut diagnostics = diagnostics
        .into_iter()
        .filter_map(|mut diagnostic| {
            // Clamp columns to the line end. Pyflakes includes the newline in its end column.
            for position in [&mut diagnostic.range.start, &mut diagnostic.range.end] {
                let line = text.slice_line(position.line as usize);
                position.character = (position.character as usize).min(line.len_utf16()) as u32;
            }
            // A point diagnostic can still mark the following character on its line.
            if diagnostic.range.start == diagnostic.range.end {
                let start = editor_position(text, diagnostic.range.start)?;
                let line = text.slice_line(start.line as usize);
                let mut chars = line.chars_at(start.character as usize);
                if let Some(ch) = chars.next().filter(|ch| *ch != '\r') {
                    diagnostic.range.end.character += ch.len_utf16() as u32;
                } else {
                    let ch = line.chars_at(start.character as usize).prev()?;
                    diagnostic.range.start.character -= ch.len_utf16() as u32;
                }
            }
            diagnostic.range = editor_range(text, diagnostic.range)?;
            Some(diagnostic)
        })
        .collect::<Vec<_>>();
    diagnostics.sort_by_key(|diagnostic| diagnostic.range.start);
    diagnostics
}

fn marked_markdown(marked: MarkedString) -> String {
    match marked {
        MarkedString::String(value) => value,
        MarkedString::LanguageString(code) => fenced(&code.language, &code.value),
    }
}

fn fenced(language: &str, value: &str) -> String {
    let language = language.trim();
    let language = if language
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "_-+#".contains(ch))
    {
        language
    } else {
        ""
    };
    let mut marker = "```".to_owned();
    while value.contains(&marker) {
        marker.push('`');
    }
    format!("{marker}{language}\n{value}\n{marker}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::SemanticTokenModifier;

    #[test]
    fn translates_semantic_tokens_after_emoji_and_skipped_tokens() {
        let text = Rope::from("😀 甲 call()\n下一行");
        let legend = SemanticTokensLegend {
            token_types: ["unknown", "method", "variable"]
                .into_iter()
                .map(SemanticTokenType::new)
                .collect(),
            token_modifiers: vec![SemanticTokenModifier::READONLY],
        };
        let tokens = SemanticTokens {
            result_id: None,
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 3,
                    length: 1,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 2,
                    length: 4,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 1,
                    delta_start: 1,
                    length: 2,
                    token_type: 2,
                    token_modifiers_bitset: 1,
                },
            ],
        };
        let result = translate_tokens(&text, tokens, &legend);
        assert_eq!(result.data.len(), 2);
        let call = result.data[0];
        assert_eq!((call.delta_line, call.delta_start, call.length), (0, 4, 4));
        assert_eq!(
            SEMANTIC_ROLES[(call.token_type / SEMANTIC_VARIANTS) as usize],
            "function.method"
        );
        let value = result.data[1];
        assert_eq!(
            (value.delta_line, value.delta_start, value.length),
            (1, 1, 2)
        );
        assert_eq!(
            SEMANTIC_ROLES[(value.token_type / SEMANTIC_VARIANTS) as usize],
            "constant"
        );
        assert_eq!(value.token_type % SEMANTIC_VARIANTS, READONLY);
        assert_eq!(editor_position(&text, Position::new(0, 1)), None);
    }
}
