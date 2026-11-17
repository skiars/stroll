use std::{cell::RefCell, rc::Rc, time::Duration};

use anyhow::Result;
use gpui::{App, Context, Focusable as _, Task, Window};
use gpui_component::input::{
    CompletionInput, CompletionProvider, EditorState, Rope, RopeExt, SyntaxContext,
};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, InsertTextFormat, Position, Range as LspRange,
};

use super::EditorLsp;

const DEBOUNCE: Duration = Duration::from_millis(90);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionActivation {
    Code,
    TriggerOnly,
    Link,
}

enum CompletionSession {
    Pending {
        trigger: CompletionTriggerKind,
        code_only: bool,
    },
    Incomplete,
    Complete(Vec<CompletionItem>),
}

impl CompletionSession {
    fn from_response(
        response: CompletionResponse,
        query: &CompletionQuery<'_>,
    ) -> (Vec<CompletionItem>, Self) {
        let (mut items, incomplete) = match response {
            CompletionResponse::Array(items) => (items, false),
            CompletionResponse::List(list) => (list.items, list.is_incomplete),
        };
        fn sort_key(item: &CompletionItem) -> &str {
            item.sort_text
                .as_deref()
                .filter(|text| !text.is_empty())
                .unwrap_or(&item.label)
        }
        items.sort_by(|a, b| sort_key(a).cmp(sort_key(b)));
        let matching = Self::matching_items(&items, query);
        let session = if incomplete {
            Self::Incomplete
        } else {
            Self::Complete(items)
        };
        (matching, session)
    }

    fn matching_items(
        items: &[CompletionItem],
        query: &CompletionQuery<'_>,
    ) -> Vec<CompletionItem> {
        items
            .iter()
            .filter(|item| {
                let target = item
                    .filter_text
                    .as_deref()
                    .filter(|text| !text.is_empty())
                    .unwrap_or(&item.label);
                let mut target = target.chars();
                query
                    .text
                    .slice(query.start_for_item(item)..query.caret)
                    .chars()
                    .all(|next| target.any(|ch| chars_eq_ignore_case(ch, next)))
            })
            .cloned()
            .collect()
    }

    fn rebase(items: &mut Vec<CompletionItem>, at: Position) {
        items.retain_mut(|item| {
            let shift = |position: &mut Position| {
                if position.line == at.line {
                    position.character += 1;
                }
            };
            if let Some(edit) = &mut item.text_edit {
                let extend = |range: &mut LspRange| shift(&mut range.end);
                match edit {
                    CompletionTextEdit::Edit(edit) => extend(&mut edit.range),
                    CompletionTextEdit::InsertAndReplace(edit) => {
                        extend(&mut edit.insert);
                        extend(&mut edit.replace);
                    }
                }
            }
            if let Some(edits) = &mut item.additional_text_edits {
                for edit in edits {
                    if edit.range.start >= at {
                        shift(&mut edit.range.start);
                        shift(&mut edit.range.end);
                    } else if edit.range.end > at {
                        return false;
                    }
                }
            }
            true
        });
    }
}

struct CompletionQuery<'a> {
    text: &'a Rope,
    start: usize,
    caret: usize,
    activation: CompletionActivation,
}

impl CompletionQuery<'_> {
    fn start_for_item(&self, item: &CompletionItem) -> usize {
        if self.activation != CompletionActivation::Link {
            return self.start;
        }
        let start = match &item.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => edit.range.start,
            Some(CompletionTextEdit::InsertAndReplace(edit)) => edit.insert.start,
            None => self.text.offset_to_position(self.start),
        };
        let start = self.text.position_to_offset(&start);
        self.caret
            - self
                .text
                .slice(start..self.caret)
                .chars()
                .reversed()
                .take_while(|&ch| !matches!(ch, '/' | '#'))
                .map(char::len_utf8)
                .sum::<usize>()
    }
}

fn chars_eq_ignore_case(left: char, right: char) -> bool {
    left == right || left.to_lowercase().eq(right.to_lowercase())
}

fn identifier(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '$')
}

fn prefix(text: &Rope, offset: usize) -> usize {
    offset
        - text
            .chars_at(offset)
            .reversed()
            .take_while(|&ch| identifier(ch))
            .map(char::len_utf8)
            .sum::<usize>()
}

struct CompletionSnapshot {
    revision: u64,
    caret: usize,
    start: usize,
    session: CompletionSession,
}

#[derive(Default)]
pub(super) struct CompletionCache(Rc<RefCell<Option<CompletionSnapshot>>>);

impl CompletionProvider for EditorLsp {
    fn completion_dismissed(&self) {
        self.completion.0.borrow_mut().take();
    }

    fn is_completion_trigger(&self, _: usize, _: &str, _: &mut App) -> bool {
        false
    }

    fn completions(
        &self,
        _: &Rope,
        _: usize,
        _: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        // Workspace editors use the snapshot-aware hook for both input and
        // invocation. A declined request has no legacy candidates.
        Task::ready(Ok(CompletionResponse::Array(Vec::new())))
    }

    fn prepare_completion(
        &self,
        editor: &EditorState,
        input: CompletionInput<'_>,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) -> Option<(usize, Task<Result<CompletionResponse>>)> {
        let cache = &self.completion.0;
        let previous = cache.borrow_mut().take();
        let options = self.document.completion_options()?;
        let text = editor.text();
        let caret = editor.cursor();
        let revision = editor.document_revision();
        let activation = self.completion_activation;
        let invoked = CompletionTriggerKind::INVOKED;
        let (start, kind, trigger_character, delay, code_only) = match input {
            CompletionInput::Invoked => (prefix(text, caret), invoked, None, Duration::ZERO, false),
            CompletionInput::Typed {
                text: typed,
                insertion_start,
            } => {
                let mut chars = typed.chars();
                let ch = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                if options
                    .trigger_characters
                    .as_ref()
                    .is_some_and(|triggers| triggers.iter().any(|trigger| trigger == typed))
                {
                    (
                        caret,
                        CompletionTriggerKind::TRIGGER_CHARACTER,
                        Some(typed.to_owned()),
                        Duration::ZERO,
                        false,
                    )
                } else if let Some(snapshot) = previous.filter(|snapshot| {
                    snapshot.revision.wrapping_add(1) == revision
                        && Some(snapshot.caret) == insertion_start
                }) {
                    let link_separator = activation == CompletionActivation::Link
                        && matches!(ch, '/' | '#' | '.' | '-' | '%');
                    if !identifier(ch) && !link_separator {
                        return None;
                    }
                    let start = snapshot.start;
                    let (kind, code_only) = match snapshot.session {
                        _ if link_separator => (invoked, false),
                        CompletionSession::Complete(mut all_items) => {
                            CompletionSession::rebase(
                                &mut all_items,
                                text.offset_to_position(snapshot.caret),
                            );
                            let query = CompletionQuery {
                                text,
                                start,
                                caret,
                                activation,
                            };
                            let matches = CompletionSession::matching_items(&all_items, &query);
                            *cache.borrow_mut() = Some(CompletionSnapshot {
                                revision,
                                caret,
                                start,
                                session: CompletionSession::Complete(all_items),
                            });
                            return Some((
                                start,
                                Task::ready(Ok(CompletionResponse::Array(matches))),
                            ));
                        }
                        CompletionSession::Incomplete => (
                            CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS,
                            false,
                        ),
                        CompletionSession::Pending { trigger, code_only } => {
                            let kind = if trigger == CompletionTriggerKind::TRIGGER_CHARACTER {
                                invoked
                            } else {
                                trigger
                            };
                            (kind, code_only)
                        }
                    };
                    (start, kind, None, DEBOUNCE, code_only)
                } else {
                    let start = prefix(text, caret);
                    if activation != CompletionActivation::Code
                        || !identifier(ch)
                        || !text
                            .char_at(start)
                            .is_some_and(|ch| ch.is_alphabetic() || matches!(ch, '_' | '$'))
                    {
                        return None;
                    }
                    (start, invoked, None, DEBOUNCE, true)
                }
            }
        };

        *cache.borrow_mut() = Some(CompletionSnapshot {
            revision,
            caret,
            start,
            session: CompletionSession::Pending {
                trigger: kind,
                code_only,
            },
        });
        let cache = cache.clone();
        let text = text.clone();
        let document = self.document.clone();
        let executor = cx.background_executor().clone();
        let context = CompletionContext {
            trigger_kind: kind,
            trigger_character,
        };
        let task = cx.spawn_in(window, async move |editor, cx| {
            if !delay.is_zero() {
                executor.timer(delay).await;
            }
            let current = editor.update_in(cx, |state, window, cx| {
                state.document_revision() == revision
                    && state.cursor() == caret
                    && state.focus_handle(cx).is_focused(window)
                    && state.is_editable()
                    && !state.is_composing()
                    && (!code_only || state.syntax_context_at(start) == SyntaxContext::Code)
            })?;
            if !current {
                return Ok(CompletionResponse::Array(Vec::new()));
            }
            let request = document.completions(text.clone(), caret, context);
            let (items, session) = executor
                .spawn(fetch(request, text, start, caret, activation))
                .await?;
            if let Some(snapshot) = cache.borrow_mut().as_mut().filter(|snapshot| {
                snapshot.revision == revision && snapshot.caret == caret && snapshot.start == start
            }) {
                snapshot.session = session;
            }
            Ok(CompletionResponse::Array(items))
        });
        Some((start, task))
    }
}

async fn fetch(
    request: impl Future<Output = Result<Option<CompletionResponse>>>,
    text: Rope,
    start: usize,
    caret: usize,
    activation: CompletionActivation,
) -> Result<(Vec<CompletionItem>, CompletionSession)> {
    let mut response = request
        .await?
        .unwrap_or_else(|| CompletionResponse::Array(Vec::new()));
    let items = match &mut response {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => &mut list.items,
    };
    items.retain_mut(|item| normalize_completion(&text, caret, item).is_some());
    let query = CompletionQuery {
        text: &text,
        start,
        caret,
        activation,
    };
    Ok(CompletionSession::from_response(response, &query))
}

fn normalize_completion(text: &Rope, offset: usize, item: &mut CompletionItem) -> Option<()> {
    if item.label.is_empty() || item.insert_text_format == Some(InsertTextFormat::SNIPPET) {
        return None;
    }
    let position = text.offset_to_position(offset);
    let normalize = |range: LspRange| {
        let start = super::actions::byte_offset(text, range.start)?;
        let end = super::actions::byte_offset(text, range.end)?;
        (start <= end)
            .then(|| LspRange::new(text.offset_to_position(start), text.offset_to_position(end)))
    };
    let primary_range = |range| {
        let range = normalize(range)?;
        (range.start.line == position.line
            && range.end.line == position.line
            && range.start <= position
            && position <= range.end)
            .then_some(range)
    };
    if let Some(edit) = &mut item.text_edit {
        match edit {
            CompletionTextEdit::Edit(edit) => edit.range = primary_range(edit.range)?,
            CompletionTextEdit::InsertAndReplace(edit) => {
                edit.insert = primary_range(edit.insert)?;
                edit.replace = primary_range(edit.replace)?;
                if edit.insert.start != edit.replace.start || edit.insert.end > edit.replace.end {
                    return None;
                }
            }
        }
    }
    if let Some(edits) = &mut item.additional_text_edits {
        for edit in edits {
            edit.range = normalize(edit.range)?;
        }
    }
    Some(())
}
