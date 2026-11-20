use super::*;
use serde::{Deserialize, Serialize};
use std::{io::Write, process::Stdio};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ByteSelection {
    pub anchor: usize,
    pub head: usize,
}

impl ByteSelection {
    pub(super) fn caret(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Event {
    Select {
        file: usize,
        selections: Vec<ByteSelection>,
    },
    Edit {
        file: usize,
        splices: Vec<Splice>,
    },
    Input {
        text: &'static str,
    },
    Undo,
    Redo,
    Publish,
}

impl From<Action> for Event {
    fn from(action: Action) -> Self {
        match action {
            Action::Input(text) => Self::Input { text },
            Action::Undo => Self::Undo,
            Action::Redo => Self::Redo,
            Action::Publish => Self::Publish,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct Transaction {
    before: String,
    after: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct BufferFrame {
    text: String,
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(super) struct Frame {
    buffers: [BufferFrame; 2],
    pub target: Option<usize>,
    pub selections: Vec<Option<ByteSelection>>,
}

#[derive(Deserialize)]
struct SavedTransaction {
    group: Option<u64>,
    changes: Vec<SavedChange>,
}

#[derive(Deserialize)]
struct SavedChange {
    old_start: usize,
    new_start: usize,
    before: String,
    after: String,
}

fn replay(text: &mut String, start: usize, removed: &str, inserted: &str) {
    let end = start.checked_add(removed.len()).unwrap();
    assert_eq!(
        text.get(start..end),
        Some(removed),
        "Invalid UTF-8 range or transaction mismatch at bytes {start}..{end}"
    );
    text.replace_range(start..end, inserted);
}

impl BufferFrame {
    // Compare the endpoints of each transaction in stack order. The editor
    // may record one broad splice where the model records exact changed spans.
    fn from_history(
        text: String,
        (undo, redo): (Vec<SavedTransaction>, Vec<SavedTransaction>),
    ) -> Self {
        let mut past = text.clone();
        let undo = undo
            .into_iter()
            .rev()
            .map(|entry| {
                assert_eq!(entry.group, None, "The trace uses per-file transactions");
                let after = past.clone();
                for change in entry.changes.into_iter().rev() {
                    replay(&mut past, change.new_start, &change.after, &change.before);
                }
                Transaction {
                    before: past.clone(),
                    after,
                }
            })
            .collect();
        let mut future = text.clone();
        let redo = redo
            .into_iter()
            .rev()
            .map(|entry| {
                assert_eq!(entry.group, None, "The trace uses per-file transactions");
                let before = future.clone();
                for change in entry.changes {
                    replay(&mut future, change.old_start, &change.before, &change.after);
                }
                Transaction {
                    before,
                    after: future.clone(),
                }
            })
            .collect();
        Self { text, undo, redo }
    }
}

impl Model {
    pub(super) fn trace_frame(&self) -> Frame {
        let stack = |entries: &[Vec<Change>]| {
            entries
                .iter()
                .map(|entry| SavedTransaction {
                    group: None,
                    changes: entry
                        .iter()
                        .map(|change| SavedChange {
                            old_start: change.start,
                            new_start: change.start,
                            before: change.removed.clone(),
                            after: change.inserted.clone(),
                        })
                        .collect(),
                })
                .collect()
        };
        Frame {
            buffers: self.buffers.each_ref().map(|buffer| {
                BufferFrame::from_history(
                    buffer.text.clone(),
                    (stack(&buffer.undo), stack(&buffer.redo)),
                )
            }),
            target: Some(self.active),
            selections: self
                .carets
                .iter()
                .map(|&offset| Some(ByteSelection::caret(offset)))
                .collect(),
        }
    }
}

impl Review<'_> {
    fn select_bytes(&mut self, file: usize, selections: &[ByteSelection]) {
        self.visual.update(|window, cx| {
            let panel = self.panel.read(cx);
            let text = self.editor.read(cx).text().to_string();
            let projected = |byte| {
                let expected = Some((PathBuf::from(FILES[file]), byte));
                text.char_indices()
                    .map(|(offset, _)| offset)
                    .chain([text.len()])
                    .find(|&offset| panel.review_test_source_offset(offset) == expected)
                    .unwrap_or_else(|| panic!("Source byte {byte} in {} is hidden", FILES[file]))
            };
            let selections = selections
                .iter()
                .map(|selection| (projected(selection.anchor), projected(selection.head)))
                .collect::<Vec<_>>();
            let [(anchor, head), others @ ..] = selections.as_slice() else {
                panic!("The headless trace requires a primary selection");
            };
            self.editor.update(cx, |editor, cx| {
                editor.focus(window, cx);
                editor.set_selected_range_with_direction(
                    *anchor.min(head)..*anchor.max(head),
                    anchor > head,
                    cx,
                );
                for &(anchor, head) in others {
                    assert_eq!(anchor, head, "Secondary selections are observed as carets");
                    editor.add_selection(head..head, cx);
                }
            });
        });
    }

    fn trace_frame(&mut self, focused: bool) -> Frame {
        self.visual.update(|window, cx| {
            if focused {
                window.draw(cx).clear(cx);
            }
            let panel = self.panel.read(cx);
            let workspace = panel.workspace.upgrade().unwrap();
            let files = &workspace.read(cx).files;
            let buffers = FILES.map(|name| {
                let file = &files[Path::new(name)];
                let editor = file.editor.read(cx);
                let text = editor.text().to_string();
                assert_eq!(file.committed.to_string(), text);
                let snapshot = editor.edit_history_snapshot().unwrap().unwrap();
                BufferFrame::from_history(text, serde_json::from_slice(&snapshot).unwrap())
            });
            if !focused {
                return Frame {
                    buffers,
                    target: None,
                    selections: Vec::new(),
                };
            }
            let map = |offset| panel.review_test_source_offset(offset);
            let editor = self.editor.read(cx);
            let target = panel.editing_path(window, cx).map(|path| {
                FILES
                    .iter()
                    .position(|file| Path::new(file) == path)
                    .unwrap()
            });
            if let Some(hidden) = panel.review_test_hidden_selections(cx) {
                let selections = hidden
                    .into_iter()
                    .map(|[anchor, head]| {
                        let ((anchor_file, anchor), (head_file, head)) = anchor.zip(head)?;
                        assert_eq!(anchor_file, head_file);
                        Some(ByteSelection { anchor, head })
                    })
                    .collect();
                let mut frame = Frame {
                    buffers,
                    target,
                    selections,
                };
                normalize(&mut frame, true);
                return frame;
            }
            let selection = editor.selected_range();
            let (start, end) = (map(selection.start), map(selection.end));
            let main = start
                .zip(end)
                .map(|((anchor_file, start), (head_file, end))| {
                    assert_eq!(anchor_file, head_file);
                    if editor.is_selection_reversed() {
                        ByteSelection {
                            anchor: end,
                            head: start,
                        }
                    } else {
                        ByteSelection {
                            anchor: start,
                            head: end,
                        }
                    }
                });
            let mut secondary = secondary_offsets(editor, window, cx)
                .into_iter()
                .map(|offset| {
                    map(offset).map(|(path, byte)| {
                        assert_eq!(
                            Some(path.as_path()),
                            target.map(|file| Path::new(FILES[file]))
                        );
                        ByteSelection::caret(byte)
                    })
                })
                .collect::<Vec<_>>();
            secondary.sort_unstable_by_key(|selection| {
                selection.as_ref().map(|selection| selection.head)
            });
            Frame {
                buffers,
                target,
                selections: std::iter::once(main).chain(secondary).collect(),
            }
        })
    }
}

#[derive(Debug, Serialize)]
struct Request<'a> {
    texts: [&'a str; 2],
    split: bool,
    events: Vec<Event>,
}

struct Checkpoint {
    actual: Frame,
    focused: bool,
}

pub(super) struct Trace<'a> {
    request: Request<'a>,
    frames: Vec<Checkpoint>,
    published: [String; 2],
    focused: bool,
}

fn normalize(frame: &mut Frame, focused: bool) {
    if !focused {
        frame.target = None;
        frame.selections.clear();
    } else {
        // Keep the primary selection and compare secondary carets by position.
        let tail = frame.selections.get_mut(1..).unwrap_or_default();
        tail.sort_unstable_by_key(|selection| selection.as_ref().map(|selection| selection.head));
    }
}

impl<'a> Trace<'a> {
    pub(super) fn new(texts: [&'a str; 2], split: bool, review: &mut Review<'_>) -> Self {
        let expected = Model::new(texts).trace_frame();
        let mut trace = Self {
            request: Request {
                texts,
                split,
                events: Vec::new(),
            },
            frames: Vec::new(),
            published: texts.map(str::to_owned),
            focused: false,
        };
        trace.record(review, Some(expected));
        trace
    }

    pub(super) fn step(&mut self, review: &mut Review<'_>, event: Event, model: &Model) {
        self.execute(review, event, Some(model.trace_frame()));
    }

    pub(super) fn execute(
        &mut self,
        review: &mut Review<'_>,
        event: Event,
        expected: Option<Frame>,
    ) {
        let previous = &self.frames.last().unwrap().actual;
        let pending = previous
            .buffers
            .iter()
            .zip(&self.published)
            .any(|(buffer, published)| &buffer.text != published);
        match &event {
            Event::Select { file, selections } => {
                review.select_bytes(*file, selections);
                self.focused = true;
            }
            Event::Edit { file, splices } => {
                let mut text = previous.buffers[*file].text.clone();
                for splice in splices {
                    assert!(
                        text.get(splice.range.clone()).is_some(),
                        "Invalid UTF-8 splice"
                    );
                    text.replace_range(splice.range.clone(), &splice.text);
                }
                // The legacy Rust path sees the enclosing replacement. Both
                // sides receive the same semantic transaction and final text.
                review.replace_source(*file, &text);
            }
            Event::Input { text } => review.step(Action::Input(text), pending),
            Event::Undo => review.step(Action::Undo, pending),
            Event::Redo => review.step(Action::Redo, pending),
            Event::Publish => {
                review.step(Action::Publish, pending);
                self.published = previous
                    .buffers
                    .each_ref()
                    .map(|buffer| buffer.text.clone());
            }
        }
        self.request.events.push(event);
        self.record(review, expected);
    }

    fn record(&mut self, review: &mut Review<'_>, expected: Option<Frame>) {
        let actual = review.trace_frame(self.focused);
        let step = self.request.events.len();
        if let Some(mut frame) = expected {
            normalize(&mut frame, self.focused);
            assert_eq!(actual, frame, "Reference mismatch at step {step}");
        }
        self.frames.push(Checkpoint {
            actual,
            focused: self.focused,
        });
    }

    pub(super) fn check_lean(&self, executable: &Path) {
        let output = run_lean(executable, &self.request);
        assert!(
            output.status.success(),
            "Lean error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let frames: Vec<Frame> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(frames.len(), self.frames.len());
        for (index, (mut lean, check)) in frames.into_iter().zip(&self.frames).enumerate() {
            normalize(&mut lean, check.focused);
            let step = index.checked_sub(1).map(|i| &self.request.events[i]);
            assert_eq!(
                lean, check.actual,
                "Lean/Rust mismatch at step {index}: {step:?}"
            );
        }
    }
}

fn run_lean(executable: &Path, request: &Request<'_>) -> std::process::Output {
    let mut runner = std::process::Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| {
            panic!(
                "Run lake build editor_trace in formal first. {}: {error}",
                executable.display()
            )
        });
    runner
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(request).unwrap())
        .unwrap();
    runner.wait_with_output().unwrap()
}

pub(super) fn rejects_invalid_byte_offsets(executable: &Path) {
    for event in [
        Event::Select {
            file: 0,
            selections: vec![ByteSelection::caret(1)],
        },
        Event::Edit {
            file: 0,
            splices: vec![Splice {
                range: 0..1,
                text: "!".into(),
            }],
        },
    ] {
        let request = Request {
            texts: ["é", ""],
            split: false,
            events: vec![event],
        };
        let output = run_lean(executable, &request);
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "Rejected traces publish no observed frames"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim(),
            "Invalid UTF-8 byte offset 1"
        );
    }
}
