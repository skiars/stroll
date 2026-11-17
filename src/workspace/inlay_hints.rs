use std::{
    cell::RefCell,
    ops::Range,
    rc::{Rc, Weak},
    time::Duration,
};

use gpui::{App, Entity, Task, WeakEntity};
use gpui_component::input::{EditorState, InlayHint, Rope, RopeExt};

pub(super) type Provider =
    Rc<dyn Fn(&Rope, &[Range<usize>], &App) -> Task<anyhow::Result<Vec<InlayHint>>>>;

const PAGE_LINES: usize = 64;

#[derive(PartialEq, Eq)]
struct Request {
    revision: u64,
    ranges: Vec<Range<usize>>,
}

#[derive(Default)]
struct Requests {
    enabled: bool,
    provider: Option<Provider>,
    requested: Option<Rc<Request>>,
    pending: Option<Task<()>>,
    cached: Option<(Rc<Request>, Vec<InlayHint>)>,
}

#[derive(Clone)]
pub(super) struct InlayHints {
    editor: WeakEntity<EditorState>,
    requests: Rc<RefCell<Requests>>,
}

impl InlayHints {
    pub(super) fn new(editor: &Entity<EditorState>, cx: &mut App) -> Self {
        let hints = Self {
            editor: editor.downgrade(),
            requests: Rc::default(),
        };
        let requests = Rc::downgrade(&hints.requests);
        let target = hints.editor.clone();
        editor.update(cx, |editor, _| {
            editor.set_viewport_observer(move |lines, revision, cx| {
                if let Some(requests) = requests.upgrade() {
                    Self::request(&requests, &target, lines, revision, cx);
                }
            });
        });
        hints
    }

    pub(super) fn enabled(&self) -> bool {
        self.requests.borrow().enabled
    }

    pub(super) fn set_enabled(&self, enabled: bool, cx: &mut App) {
        if self.enabled() != enabled {
            self.requests.borrow_mut().enabled = enabled;
            self.invalidate(cx);
        }
    }

    pub(super) fn set_provider(&self, provider: Option<Provider>, cx: &mut App) {
        self.requests.borrow_mut().provider = provider;
        self.invalidate(cx);
    }

    pub(super) fn invalidate(&self, cx: &mut App) {
        self.reset_request(true, cx);
    }

    pub(super) fn refresh(&self, cx: &mut App) {
        self.reset_request(false, cx);
    }

    fn reset_request(&self, clear_labels: bool, cx: &mut App) {
        let mut requests = self.requests.borrow_mut();
        let requested = requests.requested.take().is_some();
        if !requests.enabled && !requested && requests.cached.is_none() {
            return;
        }
        requests.pending = None;
        if clear_labels {
            requests.cached = None;
        }
        drop(requests);

        _ = self.editor.update(cx, |editor, cx| {
            if clear_labels {
                editor.set_inlay_hints(Vec::new(), cx);
            }
            cx.notify();
        });
    }

    pub(super) fn covers(&self, mut contains: impl FnMut(&[Range<usize>]) -> bool) -> bool {
        let requests = self.requests.borrow();
        let requested = requests
            .requested
            .as_ref()
            .is_some_and(|request| contains(&request.ranges));
        let painted = requests
            .cached
            .as_ref()
            .is_some_and(|(request, _)| contains(&request.ranges));
        requested || painted
    }

    fn request(
        requests: &Rc<RefCell<Requests>>,
        editor: &WeakEntity<EditorState>,
        lines: &[usize],
        revision: u64,
        cx: &App,
    ) {
        let mut state = requests.borrow_mut();
        if !state.enabled || lines.is_empty() {
            return;
        }
        let Some(provider) = state.provider.clone() else {
            return;
        };
        let request = Request {
            revision,
            ranges: request_ranges(lines),
        };
        if state.requested.as_deref() == Some(&request) {
            return;
        }
        let request = Rc::new(request);
        state.requested = Some(request.clone());
        let requests = Rc::downgrade(requests);
        let editor = editor.clone();
        state.pending = Some(cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_millis(110))
                .await;

            if !is_current(&requests, &request) {
                return;
            }
            let Ok(Some(task)) = editor.read_with(cx, |editor, cx| {
                let text = editor.text();
                if editor.document_revision() != revision
                    || editor.is_composing()
                    || text.len() == 0
                {
                    return None;
                }
                let line_count = text.lines_len();
                let ranges: Vec<_> = request
                    .ranges
                    .iter()
                    .map(|range| range.start..range.end.min(line_count))
                    .collect();
                Some(provider(text, &ranges, cx))
            }) else {
                if let Some(requests) = requests.upgrade() {
                    requests.borrow_mut().requested = None;
                }
                return;
            };

            let Ok(hints) = task.await else {
                return;
            };
            if !is_current(&requests, &request) {
                return;
            }
            let Some(requests) = requests.upgrade() else {
                return;
            };
            _ = editor.update(cx, |editor, cx| {
                if editor.document_revision() != revision {
                    return;
                }
                let response = (request, hints);
                let mut state = requests.borrow_mut();
                if state.cached.as_ref() != Some(&response) {
                    editor.set_inlay_hints(response.1.clone(), cx);
                    state.cached = Some(response);
                }
            });
        }));
    }
}

fn is_current(requests: &Weak<RefCell<Requests>>, request: &Rc<Request>) -> bool {
    requests.upgrade().is_some_and(|requests| {
        requests
            .borrow()
            .requested
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, request))
    })
}

fn request_ranges(lines: &[usize]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for &line in lines {
        let start = line / PAGE_LINES * PAGE_LINES;
        let end = start + PAGE_LINES;
        if let Some(last) = ranges.last_mut().filter(|last| last.end >= start) {
            last.end = end;
        } else {
            ranges.push(start..end);
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_bounded_ranges_around_folded_lines() {
        assert_eq!(request_ranges(&[2, 3, 64, 4000]), [0..128, 3968..4032]);
    }
}
