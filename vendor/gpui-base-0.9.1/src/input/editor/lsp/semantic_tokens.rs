use std::ops::Range;
use std::rc::Rc;

use anyhow::Result;
use gpui::{App, Context, HighlightStyle, SharedString, Task, Window};
use instant::Duration;
use lsp_types::{SemanticTokens, SemanticTokensLegend};
use ropey::Rope;

use crate::input::{EditorMode, HighlightStyleResolver, InputBaseState, Lsp, RopeExt};

/// A provider of semantic highlighting tokens, layered on top of the
/// built-in tree-sitter [`SyntaxHighlighter`](crate::highlighter::SyntaxHighlighter).
///
/// This is the editor counterpart of the LSP
/// `textDocument/semanticTokens/range` request (and Monaco Editor's
/// [`DocumentRangeSemanticTokensProvider`][monaco]). Like the other
/// providers on [`Lsp`](crate::input::Lsp) — `DocumentColorProvider`,
/// `HoverProvider`, … — it is installed on `InputBaseState::lsp`, fetched
/// asynchronously when the document changes, and its result is cached and
/// composed into the render pipeline. It does **not** replace the
/// tree-sitter highlighter.
///
/// # Token names and theming
///
/// Returned tokens are delta-encoded with a numeric `token_type` that
/// indexes [`legend`](Self::legend)`.token_types`. The editor resolves each
/// type *name* against the active
/// [`HighlightTheme`](crate::highlighter::HighlightTheme) at paint time —
/// the same vocabulary the tree-sitter path uses (`"keyword"`, `"comment"`,
/// `"string"`, …; `"keyword.modifier"` falls back to `"keyword"`). Because
/// the color is resolved from the name on every paint, theme switches
/// recolor semantic tokens with no provider cooperation. Token *modifiers*
/// are accepted but not currently mapped to styles.
///
/// [monaco]: https://microsoft.github.io/monaco-editor/
pub trait DocumentRangeSemanticTokensProvider {
    /// The legend naming the numeric `token_type` field of the tokens
    /// returned by [`semantic_tokens`](Self::semantic_tokens). Each entry in
    /// [`SemanticTokensLegend::token_types`] is resolved against the active
    /// [`HighlightTheme`](crate::highlighter::HighlightTheme).
    fn legend(&self) -> SemanticTokensLegend;

    /// Fetches semantic tokens for the specified byte range.
    ///
    /// textDocument/semanticTokens/range
    ///
    /// <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_semanticTokens>
    fn semantic_tokens(
        &self,
        text: &Rope,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<SemanticTokens>>;
}

impl Lsp {
    /// Retain ranges outside the edit while waiting for a new server response.
    pub(crate) fn adjust_semantic_tokens(
        &mut self,
        revision: u64,
        edited: &Range<usize>,
        inserted_len: usize,
    ) {
        if self.semantic_revision != Some(revision) {
            return;
        }

        let delta = inserted_len as isize - edited.len() as isize;
        self.semantic_tokens.retain_mut(|(range, _)| {
            if range.end <= edited.start {
                return true;
            }
            if range.start < edited.end {
                return false;
            }
            range.start = range.start.saturating_add_signed(delta);
            range.end = range.end.saturating_add_signed(delta);
            true
        });
        self.semantic_revision = Some(revision.wrapping_add(1));
    }

    /// Get semantic token styles that intersect with the visible byte range,
    /// resolving each cached token's type name against `theme`.
    ///
    /// Called on every paint. The cache is sorted by start position, so this
    /// binary-searches the small window of tokens that can touch the viewport
    /// (`O(log N + visible)`) instead of scanning the whole document — only
    /// the windowed candidates need to resolve a theme style.
    ///
    /// Returns byte ranges and styles.
    pub(crate) fn semantic_tokens_for_range(
        &self,
        text: &Rope,
        revision: u64,
        visible_range: &Range<usize>,
        theme: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        if visible_range.is_empty()
            || self.semantic_tokens.is_empty()
            || self.semantic_revision != Some(revision)
        {
            return Vec::new();
        }

        let first_line = text.offset_to_point(visible_range.start).row;
        let first_line_start = text.line_start_offset(first_line);

        // Cache is sorted by `range.start`. A token can only touch the
        // viewport if its start is before `visible_end` (upper bound) and it
        // is not on a line entirely above the viewport's first line (lower
        // bound — tokens are single-line, so an earlier line cannot reach in).
        let hi = self
            .semantic_tokens
            .partition_point(|(range, _)| range.start < visible_range.end);
        let lo = self
            .semantic_tokens
            .partition_point(|(range, _)| range.start < first_line_start);

        self.semantic_tokens[lo..hi]
            .iter()
            .filter_map(|(range, name)| {
                if range.end <= visible_range.start {
                    return None;
                }

                let style = theme.style(name.as_ref())?;
                Some((range.clone(), style))
            })
            .collect()
    }

    pub(crate) fn update_semantic_tokens(
        &mut self,
        text: &Rope,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<InputBaseState<EditorMode>>,
    ) {
        let Some(provider) = self.semantic_tokens_provider.as_ref() else {
            return;
        };

        let provider = provider.clone();
        let legend = provider.legend();
        let text = text.clone();
        // Fetch the whole document; results are cached and filtered to the
        // viewport at paint time (mirrors `update_document_colors`), so a
        // scroll never needs a refetch.
        let range = 0..text.len();
        let input_state = cx.entity();

        // debounce timer 100ms
        self._semantic_tokens_task = cx.spawn_in(window, async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;

            let task_result = cx
                .update(|window, cx| {
                    (input_state.read(cx).document_revision == revision)
                        .then(|| provider.semantic_tokens(&text, range, window, cx))
                })
                .ok()
                .flatten();

            if let Some(task) = task_result {
                if let Ok(tokens) = task.await {
                    let decoded = decode_semantic_tokens(&text, &tokens, &legend);
                    let _ = input_state.update(cx, |input_state, cx| {
                        let lsp = &mut input_state.extras.lsp;
                        if input_state.document_revision != revision
                            || !lsp
                                .semantic_tokens_provider
                                .as_ref()
                                .is_some_and(|current| Rc::ptr_eq(current, &provider))
                        {
                            return;
                        }
                        if decoded != lsp.semantic_tokens || lsp.semantic_revision != Some(revision) {
                            lsp.semantic_tokens = decoded;
                            lsp.semantic_revision = Some(revision);
                            cx.notify();
                        }
                    });
                }
            }
        });
    }
}

/// Decode the LSP delta-encoding of `tokens` into absolute
/// (byte-range, type-name) pairs, sorted by start offset.
///
/// The type name is looked up in `legend.token_types`; tokens whose
/// `token_type` index is out of bounds are skipped. Color resolution is
/// deferred to paint time so theme switches take effect without a refetch.
fn decode_semantic_tokens(
    text: &Rope,
    tokens: &SemanticTokens,
    legend: &SemanticTokensLegend,
) -> Vec<(Range<usize>, SharedString)> {
    // Resolve the legend names once; tokens then share them via cheap
    // ref-counted clones instead of allocating a String per token.
    let names: Vec<SharedString> = legend
        .token_types
        .iter()
        .map(|t| SharedString::from(t.as_str().to_owned()))
        .collect();

    let mut out = Vec::with_capacity(tokens.data.len());
    let mut line: u32 = 0;
    let mut character: u32 = 0;
    let mut line_start = 0;
    let mut line_text = text.slice_line(0);
    let line_count = text.lines_len();

    for token in &tokens.data {
        let Some(next_line) = line.checked_add(token.delta_line) else {
            break;
        };
        if next_line as usize >= line_count {
            break;
        }
        if next_line != line {
            line = next_line;
            character = token.delta_start;
            line_start = text.line_start_offset(line as usize);
            line_text = text.slice_line(line as usize);
        } else if let Some(next_character) = character.checked_add(token.delta_start) {
            character = next_character;
        } else {
            break;
        }

        let Some(name) = names.get(token.token_type as usize) else {
            continue;
        };

        let Some(end) = character.checked_add(token.length) else {
            continue;
        };
        if end as usize > line_text.len_chars() {
            continue;
        }
        let start = line_start + line_text.char_to_byte_idx(character as usize);
        let end = line_start + line_text.char_to_byte_idx(end as usize);
        if start < end {
            out.push((start..end, name.clone()));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::hsla;
    use lsp_types::{SemanticToken, SemanticTokenType, SemanticTokensLegend};

    fn legend() -> SemanticTokensLegend {
        SemanticTokensLegend {
            token_types: vec![SemanticTokenType::KEYWORD, SemanticTokenType::COMMENT],
            token_modifiers: vec![],
        }
    }

    struct TestTheme;

    impl HighlightStyleResolver for TestTheme {
        fn style(&self, name: &str) -> Option<HighlightStyle> {
            (name == "keyword" || name == "comment").then_some(HighlightStyle {
                color: Some(hsla(0.5, 0.5, 0.5, 1.)),
                ..Default::default()
            })
        }
    }

    #[test]
    fn test_decode_semantic_tokens_delta() {
        // Two tokens: "keyword" at (0,0..4) and "comment" at (1,2..7).
        let tokens = SemanticTokens {
            result_id: None,
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 4,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 1,
                    delta_start: 2,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        let text = Rope::from("test\n😺écomment\n");
        let decoded = decode_semantic_tokens(&text, &tokens, &legend());
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].0, 0..4);
        assert_eq!(decoded[0].1.as_ref(), "keyword");
        assert_eq!(decoded[1].0, 11..16);
        assert_eq!(decoded[1].1.as_ref(), "comment");
    }

    #[test]
    fn test_decode_skips_out_of_legend_index() {
        let tokens = SemanticTokens {
            result_id: None,
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 3,
                token_type: 99, // out of legend bounds
                token_modifiers_bitset: 0,
            }],
        };
        assert!(decode_semantic_tokens(&Rope::from("foo"), &tokens, &legend()).is_empty());
    }

    #[test]
    fn test_for_range_resolves_and_windows() {
        let text = Rope::from("SELECT * FROM users\n-- a comment line\n");
        let theme = TestTheme;

        let mut lsp = Lsp::default();
        lsp.semantic_tokens = vec![
            (0..6, SharedString::from("keyword")),
            (20..37, SharedString::from("comment")),
        ];

        // Visible range covering only line 0 (bytes 0..19).
        lsp.semantic_revision = Some(0);
        let styles = lsp.semantic_tokens_for_range(&text, 0, &(0..19), &theme);
        assert_eq!(
            styles.len(),
            1,
            "only the line-0 token should be windowed in"
        );
        assert_eq!(styles[0].0, 0..6, "keyword token maps to bytes 0..6");
        assert!(
            styles[0].1 != HighlightStyle::default(),
            "'keyword' should resolve to a non-default style on default-dark"
        );
    }

    #[test]
    fn test_for_range_binary_search_window() {
        // 100 lines of "foo bar\n" (8 bytes each), one keyword token per line
        // covering "foo" (cols 0..3).
        let text = Rope::from("foo bar\n".repeat(100).as_str());
        let theme = TestTheme;

        let mut lsp = Lsp::default();
        lsp.semantic_tokens = (0..100)
            .map(|line| {
                (
                    line * 8..line * 8 + 3,
                    SharedString::from("keyword"),
                )
            })
            .collect();

        // Only line 50 visible ("foo" at bytes 400..403). The binary-search
        // window must return exactly that one token out of 100.
        let line_bytes = "foo bar\n".len();
        let start = 50 * line_bytes;
        lsp.semantic_revision = Some(0);
        let styles = lsp.semantic_tokens_for_range(&text, 0, &(start..start + 3), &theme);
        assert_eq!(styles.len(), 1);
        assert_eq!(styles[0].0, start..start + 3);

        // Empty viewport before all tokens windows nothing in.
        assert!(
            lsp.semantic_tokens_for_range(&text, 0, &(0..0), &theme)
                .is_empty()
        );
    }

    #[test]
    fn edits_move_untouched_semantic_tokens_and_drop_overlapping_tokens() {
        let mut lsp = Lsp::default();
        lsp.semantic_revision = Some(5);
        lsp.semantic_tokens = vec![
            (0..3, "keyword".into()),
            (4..7, "comment".into()),
            (8..11, "keyword".into()),
        ];

        // Insert a newline inside the middle token, then undo the insertion.
        lsp.adjust_semantic_tokens(5, &(5..5), 1);
        assert_eq!(lsp.semantic_revision, Some(6));
        assert_eq!(lsp.semantic_tokens, [(0..3, "keyword".into()), (9..12, "keyword".into())]);
        lsp.adjust_semantic_tokens(6, &(5..6), 0);
        assert_eq!(lsp.semantic_revision, Some(7));
        assert_eq!(lsp.semantic_tokens, [(0..3, "keyword".into()), (8..11, "keyword".into())]);
    }
}
