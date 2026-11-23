use std::{
    cell::RefCell,
    ops::Range,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{HighlightStyle, SharedString, Task};
use gpui_base::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit as BaseInputEdit, InputHighlighter,
    InputHighlighterFactory,
};
use ropey::Rope;
use tree_sitter::{InputEdit, Point};

use super::{LanguageRegistry, SyntaxHighlighter};

pub fn input_highlighter_factory() -> InputHighlighterFactory {
    Rc::new(|language| {
        let registry = LanguageRegistry::singleton();
        (registry.has_parser(language) || registry.tokenizer(language).is_some()).then(|| {
            Box::new(TreeSitterInputHighlighter::new(language)) as Box<dyn InputHighlighter>
        })
    })
}

const PARSE_DEBOUNCE: Duration = Duration::from_millis(40);

struct TreeSitterInputHighlighter {
    language: SharedString,
    inner: Rc<RefCell<Option<SyntaxHighlighter>>>,
    parse_task: Option<Task<()>>,
    cancel: Arc<AtomicBool>,
}

impl Drop for TreeSitterInputHighlighter {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl TreeSitterInputHighlighter {
    fn new(language: &str) -> Self {
        Self {
            language: language.to_owned().into(),
            inner: Rc::default(),
            parse_task: None,
            cancel: Arc::default(),
        }
    }

    fn schedule<'a>(
        &mut self,
        edits: impl IntoIterator<Item = (InputEdit, &'a Rope)>,
        text: &Rope,
        folding: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<EditorState>,
    ) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::default();
        let cancel = self.cancel.clone();
        let inner = self.inner.clone();
        let language = self.language.clone();
        let text = text.clone();
        let (previous, injections) = if let Some(highlighter) = inner.borrow_mut().as_mut() {
            for (edit, intermediate) in edits {
                highlighter.edit_tree(Some(edit), intermediate);
            }
            (
                highlighter.tree().cloned(),
                highlighter.injection_parse_data(),
            )
        } else {
            (None, None)
        };
        let debounce = self.parse_task.is_some();
        self.parse_task = Some(cx.spawn_in(window, async move |editor, cx| {
            if debounce {
                cx.background_executor().timer(PARSE_DEBOUNCE).await;
            }
            let worker_cancel = cancel.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut highlighter = SyntaxHighlighter::new(&language);
                    highlighter.parse_background(text, previous.as_ref(), injections, || {
                        worker_cancel.load(Ordering::Relaxed)
                    })?;
                    let folds = if folding {
                        highlighter
                            .tree()
                            .map(extract_fold_ranges)
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    Some((highlighter, folds))
                })
                .await;
            if let Some((highlighter, folds)) = result
                && !cancel.load(Ordering::Relaxed)
            {
                *inner.borrow_mut() = Some(highlighter);
                let _ = editor.update(cx, |state, cx| {
                    state.apply_highlighter_fold_candidates(folds, cx)
                });
            }
        }));
    }
}

impl SyntaxHighlighter {
    pub(crate) fn update_input(
        &mut self,
        edit: Option<BaseInputEdit>,
        text: &Rope,
        timeout: Option<Duration>,
    ) -> bool {
        self.update(edit.map(to_tree_sitter_edit), text, timeout)
    }
}

impl InputHighlighter for TreeSitterInputHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    fn update(
        &mut self,
        edit: Option<BaseInputEdit>,
        text: &Rope,
        folding: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<EditorState>,
    ) {
        self.schedule(
            edit.map(|edit| (to_tree_sitter_edit(edit), text)),
            text,
            folding,
            window,
            cx,
        );
    }

    fn update_batch(
        &mut self,
        edits: &[(BaseInputEdit, Rope)],
        folding: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<EditorState>,
    ) {
        if let Some((_, text)) = edits.last() {
            self.schedule(
                edits
                    .iter()
                    .map(|(edit, text)| (to_tree_sitter_edit(*edit), text)),
                text,
                folding,
                window,
                cx,
            );
        }
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        self.inner.borrow().as_ref().map_or_else(
            || vec![(range.clone(), HighlightStyle::default())],
            |highlighter| highlighter.styles(range, resolver),
        )
    }
    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.inner
            .borrow()
            .as_ref()
            .and_then(SyntaxHighlighter::tree)
            .map(extract_fold_ranges)
            .unwrap_or_default()
    }

    fn fold_ranges_for_edit(&self, _: Range<usize>, _: &Rope) -> Vec<FoldRange> {
        // The next completed parse provides a fresh full list; the editor
        // already shifts unaffected ranges in response to the edit.
        Vec::new()
    }
}

fn to_tree_sitter_edit(edit: BaseInputEdit) -> InputEdit {
    InputEdit {
        start_byte: edit.start_byte,
        old_end_byte: edit.old_end_byte,
        new_end_byte: edit.new_end_byte,
        start_position: Point::new(edit.start_position.row, edit.start_position.column),
        old_end_position: Point::new(edit.old_end_position.row, edit.old_end_position.column),
        new_end_position: Point::new(edit.new_end_position.row, edit.new_end_position.column),
    }
}

fn extract_fold_ranges(tree: &tree_sitter::Tree) -> Vec<FoldRange> {
    extract_fold_ranges_in_range(tree, 0..usize::MAX)
}

fn extract_fold_ranges_in_range(
    tree: &tree_sitter::Tree,
    byte_range: Range<usize>,
) -> Vec<FoldRange> {
    fn collect(node: tree_sitter::Node, bytes: &Range<usize>, ranges: &mut Vec<FoldRange>) {
        if node.end_byte() <= bytes.start || node.start_byte() >= bytes.end {
            return;
        }
        let start = node.start_position().row;
        let end = node.end_position().row;
        if end.saturating_sub(start) < 2 {
            return;
        }
        ranges.push(FoldRange::new(start, end));
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect(child, bytes, ranges);
        }
    }

    let root = tree.root_node();
    let mut ranges = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        collect(child, &byte_range, &mut ranges);
    }
    ranges.sort_by_key(|range| range.start_line);
    ranges.dedup_by_key(|range| range.start_line);
    ranges
}
