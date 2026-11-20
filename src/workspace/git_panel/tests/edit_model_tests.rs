//! A bounded reference model and headless differential tests for Git review editing.
//!
//! The enumerated traces cover atomic input, per-file undo and redo, and publication
//! at explicit points. Each step compares source text, selections, and history.

use super::*;
use gpui_component::input::{Redo, RopeExt as _, Undo};
use std::ops::Range;

#[path = "lean_trace.rs"]
mod lean_trace;

use lean_trace::{ByteSelection, Event, Trace};

const FILES: [&str; 2] = ["a.txt", "b.txt"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Input(&'static str),
    Undo,
    Redo,
    Publish,
}

/// A splice uses the coordinates immediately before it is applied. The
/// supplied splices describe intent even when the actual caller submits one
/// large replacement covering the same text.
#[derive(Clone, Debug, serde::Serialize)]
struct Splice {
    range: Range<usize>,
    text: String,
}

#[derive(Clone)]
struct Change {
    start: usize,
    removed: String,
    inserted: String,
}

#[derive(Clone)]
struct Buffer {
    text: String,
    undo: Vec<Vec<Change>>,
    redo: Vec<Vec<Change>>,
}

impl Buffer {
    fn new(text: &str) -> Self {
        Self {
            text: text.into(),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    fn apply(&mut self, start: usize, removed: &str, inserted: &str) {
        assert_eq!(&self.text[start..start + removed.len()], removed);
        self.text
            .replace_range(start..start + removed.len(), inserted);
    }
}

/// The review session owns its per-file history target and the source
/// positions of its carets. Publishing can change which rows represent
/// those positions. The small model uses only collapsed selections with
/// right affinity and atomic input, without typing coalescence.
#[derive(Clone)]
struct Model {
    buffers: [Buffer; 2],
    active: usize,
    carets: Vec<usize>,
}

impl Model {
    fn new(texts: [&str; 2]) -> Self {
        Self {
            buffers: texts.map(Buffer::new),
            active: 0,
            carets: Vec::new(),
        }
    }

    fn focus(&mut self, file: usize, prefixes: &[&str]) {
        self.active = file;
        let text = &self.buffers[file].text;
        self.carets = prefixes.iter().map(|prefix| after(text, prefix)).collect();
    }

    fn apply(&mut self, file: usize, start: usize, removed: &str, inserted: &str) {
        self.buffers[file].apply(start, removed, inserted);
        if self.active == file {
            for caret in &mut self.carets {
                if *caret >= start {
                    let beyond = caret.saturating_sub(start + removed.len());
                    *caret = start + inserted.len() + beyond;
                }
            }
            let mut seen = HashSet::new();
            self.carets.retain(|caret| seen.insert(*caret));
        }
    }

    fn commit(&mut self, file: usize, splices: Vec<Splice>) {
        let mut changes = Vec::new();
        for edit in splices {
            let removed = self.buffers[file].text[edit.range.clone()].to_owned();
            let change = Change {
                start: edit.range.start,
                removed,
                inserted: edit.text,
            };
            self.apply(file, change.start, &change.removed, &change.inserted);
            changes.push(change);
        }
        self.buffers[file].undo.push(changes);
        self.buffers[file].redo.clear();
    }

    fn step(&mut self, action: Action) {
        let file = self.active;
        match action {
            Action::Input(text) => {
                let mut offsets = self.carets.clone();
                offsets.sort_unstable_by(|left, right| right.cmp(left));
                self.commit(
                    file,
                    offsets
                        .into_iter()
                        .map(|offset| Splice {
                            range: offset..offset,
                            text: text.into(),
                        })
                        .collect(),
                );
            }
            Action::Undo => {
                if let Some(changes) = self.buffers[file].undo.pop() {
                    for change in changes.iter().rev() {
                        self.apply(file, change.start, &change.inserted, &change.removed);
                    }
                    self.buffers[file].redo.push(changes);
                }
            }
            Action::Redo => {
                if let Some(changes) = self.buffers[file].redo.pop() {
                    for change in &changes {
                        self.apply(file, change.start, &change.removed, &change.inserted);
                    }
                    self.buffers[file].undo.push(changes);
                }
            }
            Action::Publish => {}
        }
    }
}

fn after(text: &str, prefix: &str) -> usize {
    let start = text
        .find(prefix)
        .unwrap_or_else(|| panic!("Missing {prefix:?} in {text:?}"));
    start + prefix.len()
}

fn repository(root: &Path, base: [&str; 2], working: [&str; 2]) {
    fs::create_dir(root).unwrap();
    setup(root);
    for (name, text) in FILES.into_iter().zip(base) {
        if !text.is_empty() {
            fs::write(root.join(name), text).unwrap();
        }
    }
    command(root, &["add", "."]);
    command(root, &["commit", "--allow-empty", "-qm", "Initial"]);
    for (name, text) in FILES.into_iter().zip(working) {
        fs::write(root.join(name), text).unwrap();
    }
    if base == ["", ""] {
        command(root, &["add", "."]);
    }
}

struct Review<'a> {
    panel: Entity<GitPanel>,
    editor: Entity<EditorState>,
    visual: &'a mut VisualTestContext,
}

impl<'a> Review<'a> {
    fn new(app: &'a mut TestAppContext, state: &Path, root: &Path, split: bool) -> Self {
        let (panel, visual) = open(app, state, root);
        // Every caret in these fixtures fits in one viewport.
        visual.simulate_resize(size(px(1280.), px(1240.)));
        let editor = visual.update(|window, cx| {
            gpui_component::Theme::update(cx, |theme| {
                theme.cursor_blink = gpui_base::input::CursorBlink::steady();
            });
            window.set_scale_factor(1.);
            panel.update(cx, |panel, cx| {
                panel.split = split;
                cx.notify();
            });
            panel.read(cx).editor.focused_column(window, cx).clone()
        });
        let mut review = Self {
            panel,
            editor,
            visual,
        };
        review.wait(
            "both source buffers and the requested layout",
            |panel, cx| {
                let workspace = panel.workspace.upgrade().unwrap();
                let files = &workspace.read(cx).files;
                let mode = panel.diff.read(cx).mode();
                FILES.iter().all(|file| files.contains_key(Path::new(file)))
                    && (mode == DiffMode::Split) == split
            },
        );
        review
    }

    fn wait(&mut self, what: &str, ready: impl Fn(&GitPanel, &App) -> bool) {
        wait_for_review(self.visual, what, |visual| {
            visual.update(|_, cx| ready(self.panel.read(cx), cx))
        });
    }

    fn wait_visible(&mut self, marker: &str) {
        wait_for_review(self.visual, marker, |visual| {
            visual.update(|_, cx| self.editor.read(cx).text().to_string().contains(marker))
        });
    }

    fn assert_visibility(&mut self, marker: &str, visible: bool) {
        self.visual.update(|_, cx| {
            assert_eq!(
                self.editor.read(cx).text().to_string().contains(marker),
                visible
            );
        });
    }

    fn set_file_collapsed(&mut self, file: usize, collapsed: bool) {
        let revision = self.revision();
        self.visual.update(|_, cx| {
            let diff = self.panel.read(cx).diff.clone();
            diff.update(cx, |diff, cx| {
                diff.set_file_collapsed(FILES[file], collapsed, cx)
            });
        });
        self.wait("file disclosure", |panel, cx| {
            panel.diff.read(cx).editor_row_revision() != revision
        });
        self.visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let panel = self.panel.read(cx);
            let visible = panel.review_test_source_offset(self.editor.read(cx).cursor());
            assert_eq!(visible.is_none(), collapsed);
        });
    }

    fn replace_source(&mut self, file: usize, text: &str) {
        self.visual.update(|window, cx| {
            let workspace = self.panel.read(cx).workspace.upgrade().unwrap();
            let editor = workspace.read(cx).files[Path::new(FILES[file])]
                .editor
                .clone();
            editor.update(cx, |editor, cx| {
                editor.edit_ranges(&[(0..editor.text().len(), text.into())], window, cx);
            });
        });
    }

    fn revision(&mut self) -> usize {
        self.visual
            .update(|_, cx| self.panel.read(cx).diff.read(cx).editor_row_revision())
    }

    fn step(&mut self, action: Action, pending: bool) {
        let revision = self.revision();
        if action == Action::Publish {
            if pending {
                self.wait("new patch revision", |panel, cx| {
                    panel.diff.read(cx).editor_row_revision() != revision
                });
            } else {
                // An edit followed by its inverse can publish the same patch.
                self.visual
                    .executor()
                    .advance_clock(Duration::from_millis(100));
                settle(self.visual);
            }
            return;
        }
        self.visual.update(|window, cx| match action {
            Action::Input(text) => {
                let key = if text == "\n" { "enter" } else { text };
                window.dispatch_keystroke(gpui::Keystroke::parse(key).unwrap(), cx);
            }
            Action::Undo => window.dispatch_action(Box::new(Undo), cx),
            Action::Redo => window.dispatch_action(Box::new(Redo), cx),
            Action::Publish => unreachable!(),
        });
        assert_eq!(
            self.revision(),
            revision,
            "{action:?} ran before patch publication"
        );
    }
}

// The editor exposes the primary caret directly. Read the remaining carets
// from the headless frame and map their positions back to UTF-8 offsets.
// This observes them without changing selections or the undo stack.
fn secondary_offsets(editor: &EditorState, window: &Window, cx: &App) -> Vec<usize> {
    let Some(active) = editor.prepaint_caret_geometry() else {
        panic!("The focused editor has a laid-out primary caret");
    };
    let primary = editor.cursor();
    let primary_bounds = editor.range_to_bounds(&(primary..primary)).unwrap();
    let origin_delta = active.cursor_bounds().origin - primary_bounds.origin;
    let viewport = editor.input_bounds();
    let text = editor.text().to_string();
    let row_range = editor.visible_row_range().unwrap();
    let start = editor.text().line_start_offset(row_range.start);
    let end = editor.text().line_start_offset(row_range.end);
    let candidates = text[start..end]
        .char_indices()
        .map(|(offset, _)| offset + start)
        .chain([end])
        .filter_map(|offset| {
            let bounds = editor.range_to_bounds(&(offset..offset))?;
            let origin = bounds.origin + origin_delta;
            Some((offset, origin.x.round().as_f32(), origin.y.as_f32()))
        })
        .collect::<Vec<_>>();
    let caret_color = cx.theme().caret;
    let mut offsets = window
        .painted_quads()
        .into_iter()
        .filter(|quad| {
            quad.background.as_solid() == Some(caret_color)
                && quad.bounds.size.width.as_f32() == active.cursor_bounds().size.width.as_f32()
                && viewport.contains(&point(
                    px(quad.bounds.origin.x.as_f32()),
                    px(quad.bounds.origin.y.as_f32()),
                ))
        })
        .map(|quad| {
            let x = quad.bounds.origin.x.as_f32();
            let y = quad.bounds.origin.y.as_f32();
            candidates
                .iter()
                .find(|&&(_, candidate_x, candidate_y)| {
                    (candidate_x - x).abs() <= 1. && (candidate_y - y).abs() < 1.
                })
                .unwrap_or_else(|| panic!("Unmatched caret at ({x}, {y}); active={active:?}"))
                .0
        })
        .collect::<Vec<_>>();
    let active_index = offsets
        .iter()
        .position(|&offset| offset == primary)
        .unwrap_or_else(|| {
            panic!("Primary caret at {primary} missing from painted carets {offsets:?}")
        });
    offsets.remove(active_index);
    offsets
}

#[::core::prelude::v1::test]
fn enumerates_edit_history_and_patch_publication() {
    enumerate_traces(None);
}

fn enumerate_traces(lean: Option<&Path>) {
    const ACTIONS: [Action; 4] = [
        Action::Input("\n"),
        Action::Undo,
        Action::Redo,
        Action::Publish,
    ];
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let texts = ["alpha-tail\nbeta-tail\n", "other file\n"];
    repository(&root, ["", ""], texts);
    for split in [false, true] {
        for first in ACTIONS {
            for second in ACTIONS {
                for third in ACTIONS {
                    let trace = [first, second, third];
                    let state = tempfile::tempdir_in(temp.path()).unwrap();
                    let mut app = test_context();
                    let mut review = Review::new(&mut app, state.path(), &root, split);
                    let mut model = Model::new(texts);
                    let mut comparison = Trace::new(texts, split, &mut review);
                    trace_focus(
                        &mut comparison,
                        &mut review,
                        &mut model,
                        0,
                        &["alpha", "beta"],
                    );
                    for action in trace {
                        trace_action(&mut comparison, &mut review, &mut model, action);
                    }
                    if let Some(executable) = lean {
                        comparison.check_lean(executable);
                    }
                }
            }
        }
    }
}

fn replacements(text: &str, pairs: &[(&str, &str)]) -> Vec<Splice> {
    let mut edits = pairs
        .iter()
        .map(|&(old, new)| {
            let end = after(text, old);
            Splice {
                range: end - old.len()..end,
                text: new.into(),
            }
        })
        .collect::<Vec<_>>();
    edits.sort_unstable_by(|left, right| right.range.start.cmp(&left.range.start));
    edits
}

fn trace_action(trace: &mut Trace<'_>, review: &mut Review<'_>, model: &mut Model, action: Action) {
    model.step(action);
    trace.step(review, action.into(), model);
}

fn trace_focus(
    trace: &mut Trace<'_>,
    review: &mut Review<'_>,
    model: &mut Model,
    file: usize,
    prefixes: &[&str],
) {
    model.focus(file, prefixes);
    let selections = model
        .carets
        .iter()
        .map(|&offset| ByteSelection::caret(offset))
        .collect();
    trace.step(review, Event::Select { file, selections }, model);
}

#[::core::prelude::v1::test]
fn cross_fold_undo_keeps_an_unchanged_anchor_for_input() {
    trace_cross_fold_anchor(None);
}

#[::core::prelude::v1::test]
fn cross_fold_redo_keeps_its_file_when_a_run_disappears() {
    trace_cross_fold_target(None);
}

#[::core::prelude::v1::test]
fn patch_publication_keeps_secondary_carets_when_context_expands() {
    trace_patch_carets(None);
}

fn trace_cross_fold_anchor(lean: Option<&Path>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let gap = (0..32)
        .map(|i| format!("context {i:02}\n"))
        .collect::<String>();
    let base = format!(
        "base heading\nleft anchor with words\nfoot sentinel\n{gap}base ending\nright anchor with words\n"
    );
    let long_before = "one heading\none pre A\none pre B\none pre C\n";
    let short_before = "two heading\ntwo pre A\n";
    let long_after = "one post A\none post B\none post C\n";
    let short_after = "two post A\ntwo post B\n";
    let long_tail = "one tail A\none tail B\none tail C\n";
    let short_tail = "two tail A\ntwo tail B\n";
    let disk = format!(
        "{long_before}left anchor with words\n{long_after}foot sentinel\n{gap}one ending\nright anchor with words\n{long_tail}"
    );
    let text = disk
        .replace(long_before, short_before)
        .replace(long_after, short_after)
        .replace(long_tail, short_tail)
        .replace("one ending", "two ending");
    let texts = [disk.as_str(), "other changed\n"];
    repository(&root, [&base, "other base\n"], texts);
    for split in [false, true] {
        let mut app = test_context();
        let state = tempfile::tempdir_in(temp.path()).unwrap();
        let mut review = Review::new(&mut app, state.path(), &root, split);
        let mut model = Model::new(texts);
        let mut trace = Trace::new(texts, split, &mut review);
        // Keep the unchanged anchor between two semantic splices. The
        // production input event reports their enclosing replacement.
        let splices = replacements(
            &disk,
            &[
                (long_before, short_before),
                (long_after, short_after),
                (long_tail, short_tail),
                ("one ending", "two ending"),
            ],
        );
        model.commit(0, splices.clone());
        assert_eq!(model.buffers[0].text, text);
        trace.step(&mut review, Event::Edit { file: 0, splices }, &model);
        trace_action(&mut trace, &mut review, &mut model, Action::Publish);
        review.assert_visibility("context 16", false);
        trace_focus(&mut trace, &mut review, &mut model, 0, &["left anchor"]);

        for action in [Action::Undo, Action::Redo, Action::Undo, Action::Input("!")] {
            trace_action(&mut trace, &mut review, &mut model, action);
        }
        if let Some(executable) = lean {
            trace.check_lean(executable);
        }
    }
}

fn trace_cross_fold_target(lean: Option<&Path>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let context = (0..40)
        .map(|i| format!("context {i:02}\n"))
        .collect::<String>();
    let (removed, kept) = context.split_at(8 * "context 00\n".len());
    let base = format!("{context}original ending\n");
    let disk = format!("{kept}disk ending with words\n");
    let prefix = format!("draft extra A\ndraft extra B\n{removed}");
    let draft = format!("{prefix}{kept}draft ending with words\n");
    let texts = [disk.as_str(), "other changed\n"];
    repository(&root, [&base, "other base\n"], texts);
    for split in [false, true] {
        let mut app = test_context();
        let state = tempfile::tempdir_in(temp.path()).unwrap();
        let mut review = Review::new(&mut app, state.path(), &root, split);
        let mut model = Model::new(texts);
        let mut trace = Trace::new(texts, split, &mut review);
        // The other file has undo history too. Compare both stacks and texts.
        let splices = replacements(texts[1], &[("changed", "draft")]);
        model.commit(1, splices.clone());
        trace.step(&mut review, Event::Edit { file: 1, splices }, &model);
        trace_action(&mut trace, &mut review, &mut model, Action::Publish);

        let splices = vec![
            Splice {
                range: kept.len()..kept.len() + "disk".len(),
                text: "draft".into(),
            },
            Splice {
                range: 0..0,
                text: prefix.clone(),
            },
        ];
        model.commit(0, splices.clone());
        assert_eq!(model.buffers[0].text, draft);
        trace.step(&mut review, Event::Edit { file: 0, splices }, &model);
        trace_action(&mut trace, &mut review, &mut model, Action::Publish);
        review.assert_visibility("context 16", false);
        trace_focus(&mut trace, &mut review, &mut model, 0, &["draft extra A"]);

        model.step(Action::Undo);
        assert_eq!(model.carets, [0]);
        trace.step(&mut review, Event::Undo, &model);
        review.assert_visibility("draft extra A", false);
        trace_action(&mut trace, &mut review, &mut model, Action::Redo);
        review.assert_visibility("draft extra A", true);
        trace_action(&mut trace, &mut review, &mut model, Action::Input("!"));
        trace_action(&mut trace, &mut review, &mut model, Action::Publish);

        // A layout publication can temporarily hide every editable row in
        // the active file. Its history target and source caret still survive.
        review.set_file_collapsed(0, true);
        for action in [Action::Publish, Action::Undo, Action::Redo, Action::Publish] {
            trace_action(&mut trace, &mut review, &mut model, action);
        }
        review.set_file_collapsed(0, false);
        trace_action(&mut trace, &mut review, &mut model, Action::Publish);
        if let Some(executable) = lean {
            trace.check_lean(executable);
        }
    }
}

fn trace_patch_carets(lean: Option<&Path>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let base = (0..30)
        .map(|i| format!("line {i:02} with words\n"))
        .collect::<String>();
    let disk = base.replace("line 14", "change 14");
    let texts = [disk.as_str(), "other changed\n"];
    repository(&root, [&base, "other base\n"], texts);
    for split in [false, true] {
        let mut app = test_context();
        let state = tempfile::tempdir_in(temp.path()).unwrap();
        let mut review = Review::new(&mut app, state.path(), &root, split);
        let mut model = Model::new(texts);
        let prefixes = ["line 11", "line 17"];
        review.wait_visible(prefixes[1]);
        review.assert_visibility("line 08", false);
        let mut trace = Trace::new(texts, split, &mut review);
        trace_focus(&mut trace, &mut review, &mut model, 0, &prefixes);
        trace_action(&mut trace, &mut review, &mut model, Action::Input("?"));
        trace_action(&mut trace, &mut review, &mut model, Action::Publish);
        review.assert_visibility("line 08", true);
        for action in [Action::Input("\n"), Action::Undo, Action::Redo] {
            trace_action(&mut trace, &mut review, &mut model, action);
        }
        if let Some(executable) = lean {
            trace.check_lean(executable);
        }
    }
}

#[::core::prelude::v1::test]
fn explicit_focus_selects_which_file_history_to_replay() {
    trace_file_focus(None);
}

fn trace_file_focus(lean: Option<&Path>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let texts = ["alpha-tail\nbeta-tail\n", "gamma-tail\ndelta-tail\n"];
    repository(&root, ["", ""], texts);
    for split in [false, true] {
        let mut app = test_context();
        let state = tempfile::tempdir_in(temp.path()).unwrap();
        let mut review = Review::new(&mut app, state.path(), &root, split);
        let mut model = Model::new(texts);
        let mut trace = Trace::new(texts, split, &mut review);
        for (file, prefixes, actions) in [
            (0, ["alpha", "beta"], &[Action::Input("\n")][..]),
            (
                1,
                ["gamma", "delta"],
                &[Action::Input("\n"), Action::Undo, Action::Redo],
            ),
            (0, ["alpha", "beta"], &[Action::Undo, Action::Publish]),
        ] {
            trace_focus(&mut trace, &mut review, &mut model, file, &prefixes);
            for &action in actions {
                trace_action(&mut trace, &mut review, &mut model, action);
            }
        }
        if let Some(executable) = lean {
            trace.check_lean(executable);
        }
    }
}

#[::core::prelude::v1::test]
#[ignore = "requires the Lean editor_trace executable"]
fn lean_headless_differential() {
    let executable = std::env::var_os("STROLL_LEAN_TRACE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("formal/.lake/build/bin")
                .join(format!("editor_trace{}", std::env::consts::EXE_SUFFIX))
        });
    trace_cross_fold_anchor(Some(&executable));
    trace_cross_fold_target(Some(&executable));
    trace_patch_carets(Some(&executable));
    trace_file_focus(Some(&executable));
    lean_trace::rejects_invalid_byte_offsets(&executable);

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let texts = ["é文🙂-tail\nβ-keep\n", "other\n"];
    repository(&root, ["", ""], texts);
    for split in [false, true] {
        let mut app = test_context();
        let state = tempfile::tempdir_in(temp.path()).unwrap();
        let mut review = Review::new(&mut app, state.path(), &root, split);
        let mut trace = Trace::new(texts, split, &mut review);
        for event in [
            Event::Edit {
                file: 1,
                splices: vec![
                    Splice {
                        range: 0..0,
                        text: "文".into(),
                    },
                    Splice {
                        range: 3..4,
                        text: "é".into(),
                    },
                ],
            },
            Event::Publish,
            Event::Select {
                file: 0,
                selections: vec![
                    ByteSelection {
                        anchor: after(texts[0], "é文🙂"),
                        head: after(texts[0], "é"),
                    },
                    ByteSelection::caret(after(texts[0], "β")),
                ],
            },
            Event::Input { text: "界" },
            Event::Publish,
            Event::Undo,
            Event::Redo,
            Event::Publish,
        ] {
            trace.execute(&mut review, event, None);
        }
        trace.check_lean(&executable);
    }
    enumerate_traces(Some(&executable));
}
