use super::*;
use crate::workspace::line_alignment::equal_line_ranges;
use gpui_component::{
    ThemeStyled as _,
    input::{InputEditsEvent, RopeExt as _},
    tooltip::{ManagedTooltipExt as _, Positioner, TooltipRequest},
};
use similar::DiffableStr;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const NEARBY_LINES: usize = 5;

#[derive(Clone, PartialEq, Eq)]
struct Target {
    head: String,
    path: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct BufferVersion {
    revision: u64,
    disk: [u8; 32],
}

struct LineMap {
    // Each entry maps an unchanged current line to a one-based baseline line.
    lines: Vec<Option<usize>>,
}

impl LineMap {
    fn between(base: &str, text: &str) -> Self {
        let mut lines = vec![None; text.bytes().filter(|&byte| byte == b'\n').count() + 1];
        let deadline = Instant::now() + Duration::from_millis(150);
        for (old, new) in
            equal_line_ranges(&base.tokenize_lines(), &text.tokenize_lines(), deadline)
        {
            for (old, new) in old.zip(new) {
                lines[new] = Some(old + 1);
            }
        }
        Self { lines }
    }

    fn get(&self, row: usize) -> Option<usize> {
        self.lines.get(row).copied().flatten()
    }
}

#[derive(Clone)]
struct Baseline {
    text: Arc<str>,
    total: usize,
}

struct Mapping {
    saved: Option<LineMap>,
    draft: Option<LineMap>,
}

impl Mapping {
    fn new(baseline: &str, saved: &Rope, text: &Rope) -> Self {
        let saved_text = saved.to_string();
        let text = (saved != text).then(|| text.to_string());
        Self {
            saved: (baseline != saved_text).then(|| LineMap::between(baseline, &saved_text)),
            draft: text
                .as_deref()
                .map(|text| LineMap::between(&saved_text, text)),
        }
    }

    fn line(&self, row: usize, total: usize) -> Line {
        let saved = self
            .draft
            .as_ref()
            .map_or(Some(row + 1), |map| map.get(row));
        if let Some(saved) = saved {
            let original = self
                .saved
                .as_ref()
                .map_or(Some(saved), |map| map.get(saved - 1));
            if let Some(original) = original.filter(|line| *line <= total) {
                Line::Committed(original)
            } else {
                Line::Local
            }
        } else {
            Line::Unsaved
        }
    }
}

enum Line {
    Committed(usize),
    Local,
    Unsaved,
}

#[derive(Default)]
pub(super) struct Blame {
    target: Option<Target>,
    baseline: Option<Option<Baseline>>,
    mapping: Option<(BufferVersion, Mapping)>,
    cache: BTreeMap<usize, Option<Arc<git::BlameLine>>>,
    shown: Option<Presentation>,
    pending: bool,
    task: Option<Task<()>>,
}

struct Presentation {
    row: usize,
    tracked: u64,
    resolved: BufferVersion,
    label: SharedString,
    commit: Option<Arc<git::BlameLine>>,
}

impl Blame {
    pub(super) fn track_edits(&mut self, event: &InputEditsEvent, editor: &EditorState) {
        let Some(shown) = self.shown.as_mut() else {
            return;
        };
        let revision = editor.document_revision();
        if shown.tracked == revision {
            return;
        }
        for (edit, _) in event.edits() {
            let (start, old_end, new_end) = (
                edit.start_position,
                edit.old_end_position,
                edit.new_end_position,
            );
            if shown.row > old_end.row {
                shown.row = new_end.row + (shown.row - old_end.row);
            } else if shown.row >= start.row {
                if shown.row == old_end.row && old_end.column == 0 && new_end.column == 0 {
                    shown.row = new_end.row;
                } else if start.row != old_end.row || start.row != new_end.row {
                    self.shown = None;
                    return;
                }
            }
        }
        if event
            .edits()
            .last()
            .is_some_and(|(_, text)| text == editor.text())
        {
            shown.tracked = revision;
        }
    }

    fn update_presentation(&mut self, row: Option<usize>, version: BufferVersion) {
        let Some(row) = row else {
            self.shown = None;
            return;
        };
        if self.shown.as_ref().is_some_and(|shown| {
            shown.tracked == version.revision && shown.row != row
                || shown.resolved.disk != version.disk
        }) {
            self.shown = None;
        }
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.row == row && shown.resolved == version)
        {
            return;
        }
        let Some((_, mapping)) = self
            .mapping
            .as_ref()
            .filter(|(mapped, _)| *mapped == version)
        else {
            return;
        };
        let Some(baseline) = self.baseline.as_ref().and_then(Option::as_ref) else {
            return;
        };
        let (label, commit) = match mapping.line(row, baseline.total) {
            Line::Local => (t!("git.blame_local").into(), None),
            Line::Unsaved => (t!("git.blame_unsaved").into(), None),
            Line::Committed(line) => match self.cache.get(&line) {
                Some(Some(line)) => (
                    format!(
                        "{} · {} · {}",
                        line.author,
                        relative_date(line.timestamp),
                        line.subject
                    )
                    .into(),
                    Some(line.clone()),
                ),
                Some(None) => (t!("git.blame_unavailable").into(), None),
                None => return,
            },
        };
        self.shown = Some(Presentation {
            row,
            tracked: version.revision,
            resolved: version,
            label,
            commit,
        });
    }
}

fn version(file: &OpenFile, cx: &App) -> BufferVersion {
    BufferVersion {
        revision: file.editor.read(cx).document_revision(),
        disk: file.document.draft_baseline(),
    }
}

fn current_line(editor: &EditorState) -> Option<usize> {
    if !editor.selected_range().is_empty() || editor.is_composing() {
        return None;
    }
    let text = editor.text();
    let len = text.len();
    if len == 0 || (editor.cursor() == len && text.byte(len - 1) == b'\n') {
        return None;
    }
    Some(editor.cursor_position().line as usize)
}

impl Workspace {
    fn open_blame_commit(
        &mut self,
        commit: &git::BlameLine,
        source: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.switch_view(WorkspaceView::Git, window, cx);
        if self.view == WorkspaceView::Git
            && let Some(panel) = &self.git
        {
            panel.update(cx, |panel, cx| {
                panel.open_blame_commit(commit, source, window, cx)
            });
        }
    }

    pub(in crate::workspace) fn refresh_editor_blame(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view != WorkspaceView::Editor {
            return;
        }
        let (Some(repo), Some(path)) = (
            self.repository.clone(),
            self.tabs.active().map(Path::to_owned),
        ) else {
            return;
        };
        let Some(file) = self.files.get_mut(&path) else {
            return;
        };
        let backend = repo.git_backend();
        let target = file
            .git
            .changes
            .as_ref()
            .filter(|_| {
                !file.missing
                    && !file.document.conflict
                    && !file.document.mixed_newlines
                    && !file.git.stale
                    && backend.supports_blame()
            })
            .and_then(|changes| {
                let git_path = git_path(&repo, &path)?;
                Some(Target {
                    head: changes.version.head.clone()?,
                    path: changes.version.blame_path(&git_path)?.to_owned(),
                })
            });
        if file.git.blame.target != target {
            file.git.blame = Blame {
                target: target.clone(),
                ..Blame::default()
            };
        }
        let Some(target) = target else { return };
        let editor = file.editor.read(cx);
        let row = current_line(editor);
        let version = version(file, cx);
        let blame = &mut file.git.blame;
        blame.update_presentation(row, version);
        let Some(row) = row else { return };
        if blame.pending {
            return;
        }

        let request = if blame.baseline.is_none()
            || (blame.baseline.as_ref().is_some_and(Option::is_some)
                && blame
                    .mapping
                    .as_ref()
                    .is_none_or(|(mapped, _)| *mapped != version))
        {
            let baseline = blame.baseline.clone();
            let (saved, text) = (file.saved.clone(), editor.text().clone());
            let target = target.clone();
            cx.background_spawn(async move {
                let baseline = baseline.unwrap_or_else(|| {
                    backend
                        .blame_baseline(&target.path, &target.head)
                        .ok()
                        .flatten()
                        .map(|text| Baseline {
                            total: line_count(&text),
                            text: Arc::from(text),
                        })
                });
                let mapping = baseline
                    .as_ref()
                    .map(|base| (version, Mapping::new(&base.text, &saved, &text)));
                Reply::Mapping(baseline, mapping)
            })
        } else {
            let Some(baseline) = blame.baseline.as_ref().and_then(Option::as_ref) else {
                return;
            };
            let total = baseline.total;
            let Some((_, mapping)) = &blame.mapping else {
                return;
            };
            let Line::Committed(line) = mapping.line(row, total) else {
                return;
            };
            if blame.cache.contains_key(&line) {
                return;
            }
            let first = line.saturating_sub(NEARBY_LINES).max(1);
            let last = (line + NEARBY_LINES).min(total);
            let target = target.clone();
            cx.background_spawn(async move {
                let lines = backend
                    .blame_lines(&target.path, &target.head, first, last)
                    .unwrap_or_default();
                Reply::Lines(first..=last, lines)
            })
        };
        let source = file.editor.clone();
        blame.pending = true;
        blame.task = Some(cx.spawn_in(window, async move |this, cx| {
            let reply = request.await;
            _ = this.update_in(cx, move |this, window, cx| {
                let Some(file) = this
                    .files
                    .get_mut(&path)
                    .filter(|file| file.editor == source)
                else {
                    return;
                };
                let blame = &mut file.git.blame;
                if blame.target.as_ref() != Some(&target) {
                    return;
                }
                blame.pending = false;
                match reply {
                    Reply::Mapping(base, map) => {
                        blame.baseline = Some(base);
                        blame.mapping = map;
                    }
                    Reply::Lines(range, lines) => {
                        for line in range {
                            blame.cache.entry(line).or_insert(None);
                        }
                        blame.cache.extend(
                            lines
                                .into_iter()
                                .map(|line| (line.line, Some(Arc::new(line)))),
                        );
                    }
                }
                this.refresh_editor_blame(window, cx);
                cx.notify();
            });
        }));
    }
}

enum Reply {
    Mapping(Option<Baseline>, Option<(BufferVersion, Mapping)>),
    Lines(std::ops::RangeInclusive<usize>, Vec<git::BlameLine>),
}

fn line_count(text: &str) -> usize {
    text.bytes().filter(|&byte| byte == b'\n').count() + usize::from(!text.ends_with('\n'))
}

pub(in crate::workspace) fn hint(
    file: &OpenFile,
    workspace: WeakEntity<Workspace>,
    font_size: Pixels,
    font: SharedString,
    colors: Palette,
    cx: &App,
) -> Option<AnyElement> {
    let editor = file.editor.read(cx);
    let row = current_line(editor)?;
    let shown = file.git.blame.shown.as_ref()?;
    let version = version(file, cx);
    if shown.row != row || shown.tracked != version.revision || shown.resolved.disk != version.disk
    {
        return None;
    }
    let commit = (shown.resolved == version)
        .then(|| shown.commit.clone())
        .flatten();
    let label = shown.label.clone();
    let editor = file.editor.clone();
    let id = SharedString::from(format!(
        "editor-blame:{:?}:{row}:{}",
        editor.entity_id(),
        commit.as_ref().map_or("", |commit| commit.oid.as_str())
    ));
    Some(
        deferred(
            canvas(
                move |_, window, cx| {
                    let editor = editor.read(cx);
                    let viewport = editor.input_bounds();
                    let (bounds, baseline) = editor.cursor_line_end_geometry()?;
                    let gap = px(16.);
                    let width = bounds.size.width - gap - px(24.);
                    if width < px(100.) {
                        return None;
                    }
                    let size = font_size * 0.93;
                    let run = [TextRun {
                        len: label.len(),
                        font: gpui::font(font),
                        color: colors.muted.opacity(0.78),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }];
                    let text_system = window.text_system();
                    let midpoint = bounds.size.height / 2.;
                    let baseline = if baseline == bounds.top() + midpoint
                        && editor
                            .text()
                            .slice_line(row)
                            .chars()
                            .all(char::is_whitespace)
                    {
                        // The empty shaped row has no glyph metrics. Use the
                        // configured editor font for the annotation on that row.
                        let space = text_system.layout_line(
                            " ",
                            font_size,
                            &[TextRun {
                                len: 1,
                                ..run[0].clone()
                            }],
                            None,
                        );
                        bounds.top() + midpoint + (space.ascent - space.descent) / 2.
                    } else {
                        baseline
                    };
                    let line = text_system.shape_line(label.clone(), size, &run, None);
                    let line = if line.width() <= width {
                        line
                    } else {
                        let (text, runs) = cx
                            .text_system()
                            .line_wrapper(run[0].font.clone(), size)
                            .truncate_line(label, width, "…", &run, TruncateFrom::End);
                        text_system.shape_line(text, size, &runs, None)
                    };
                    let offset =
                        (bounds.size.height - line.ascent - line.descent) / 2. + line.ascent;
                    let mut element = div()
                        .id(id)
                        .w(line.width().min(width))
                        .h(bounds.size.height)
                        .cursor_text()
                        .debug_selector(|| "editor-blame".into())
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |area, (), window, cx| {
                                    let origin = point(area.left(), baseline - offset);
                                    #[cfg(test)]
                                    tests::PAINTED_BASELINE.set(Some(origin.y + offset));
                                    let _ = line.paint(
                                        origin,
                                        bounds.size.height,
                                        TextAlign::Left,
                                        None,
                                        window,
                                        cx,
                                    );
                                },
                            )
                            .size_full(),
                        )
                        .when_some(commit, |label, commit| {
                            label.managed_interactive_tooltip(move |bounds, _, cx| {
                                let card = cx.new(|_| CommitCard {
                                    commit: commit.clone(),
                                    row,
                                    workspace: workspace.clone(),
                                    viewport: viewport.size,
                                    copied: false,
                                });
                                Some(
                                    TooltipRequest::new(bounds, move |_, _| card.clone().into())
                                        .with_show_delay(Duration::from_millis(500))
                                        .with_restart_delay(true)
                                        .with_positioner(move || {
                                            Positioner::side(bounds)
                                                .placement(Placement::Bottom)
                                                .within(viewport)
                                                .margin(px(6.))
                                        }),
                                )
                            })
                        })
                        .into_any_element();
                    element.prepaint_as_root(
                        point(bounds.left() + gap, bounds.top()),
                        AvailableSpace::min_size(),
                        window,
                        cx,
                    );
                    Some(element)
                },
                |_, element, window, cx| {
                    if let Some(mut element) = element {
                        element.paint(window, cx);
                    }
                },
            )
            .absolute(),
        )
        .into_any_element(),
    )
}

struct CommitCard {
    commit: Arc<git::BlameLine>,
    row: usize,
    workspace: WeakEntity<Workspace>,
    viewport: Size<Pixels>,
    copied: bool,
}

impl Render for CommitCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let line = self.commit.clone();
        let date = chrono::DateTime::from_timestamp(line.timestamp, 0)
            .map(|date| {
                let zone = line.timezone.parse::<i32>().ok().and_then(|zone| {
                    chrono::FixedOffset::east_opt((zone / 100 * 60 + zone % 100) * 60)
                });
                zone.map_or_else(
                    || date.to_rfc3339(),
                    |zone| date.with_timezone(&zone).to_rfc3339(),
                )
            })
            .unwrap_or_default();
        let card = v_flex()
            .id("editor-blame-details")
            .debug_selector(|| "editor-blame-details".into())
            .w(px(392.).min(self.viewport.width - px(12.)))
            .max_h((self.viewport.height - px(12.)).max(px(1.)))
            .overflow_y_scroll()
            .gap_2()
            .p_3()
            .text_sm()
            .child(div().font_semibold().truncate().child(line.subject.clone()))
            .child(
                div()
                    .truncate()
                    .child(format!("{} · {}", line.author, date)),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(line.oid.get(..12).unwrap_or(&line.oid).to_owned()),
            )
            .child(div().truncate().child(t!(
                "git.blame_location",
                line = self.row + 1,
                path = line.path,
                original = line.original
            )))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(
                        [
                            ("blame-diff", t!("git.blame_diff"), false),
                            ("blame-source", t!("git.blame_source"), true),
                        ]
                        .map(|(id, label, source)| {
                            div().debug_selector(move || id.into()).child(
                                quiet_button(id).small().label(label).on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        let commit = this.commit.clone();
                                        _ = this.workspace.update(cx, |workspace, cx| {
                                            workspace.open_blame_commit(&commit, source, window, cx)
                                        });
                                    },
                                )),
                            )
                        }),
                    )
                    .child(
                        div().debug_selector(|| "blame-copy".into()).child(
                            quiet_button("blame-copy")
                                .small()
                                .label(if self.copied {
                                    t!("git.blame_copied")
                                } else {
                                    t!("git.blame_copy")
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        this.commit.oid.clone(),
                                    ));
                                    this.copied = true;
                                    cx.notify();
                                })),
                        ),
                    ),
            );
        card.popover_style(cx)
    }
}

fn relative_date(timestamp: i64) -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let minutes = elapsed.saturating_sub(timestamp).max(0) / 60;
    match minutes {
        0 => t!("git.blame_now"),
        1..60 => t!("git.blame_minutes", count = minutes),
        60..1440 => t!("git.blame_hours", count = minutes / 60),
        _ => t!("git.blame_days", count = minutes / 1440),
    }
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::Root;

    thread_local! {
        pub(super) static PAINTED_BASELINE: std::cell::Cell<Option<Pixels>> = const { std::cell::Cell::new(None) };
    }

    #[::core::prelude::v1::test]
    fn maps_shifted_lines_across_disk_and_draft() {
        let base = "first\nsecond\nlast\n";
        let saved = Rope::from("saved edit\nsecond\nlast\n");
        let draft = Rope::from("new draft line\nsaved edit\nsecond\nlast\n");
        let mapping = Mapping::new(base, &saved, &draft);
        assert!(matches!(mapping.line(0, 3), Line::Unsaved));
        assert!(matches!(mapping.line(1, 3), Line::Local));
        assert!(matches!(mapping.line(2, 3), Line::Committed(2)));
        assert!(matches!(mapping.line(3, 3), Line::Committed(3)));
    }

    #[::core::prelude::v1::test]
    fn editor_blame_hover_tracks_the_current_committed_line() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.name", "History Author"]);
        run_git(&["config", "user.email", "history@example.invalid"]);
        run_git(&["config", "commit.gpgsign", "false"]);
        let path = PathBuf::from("source.txt");
        let source = (0..80)
            .map(|line| match line {
                10 => "\n".into(),
                11 => "   \n".into(),
                12 => format!("{}tail\n", "long ".repeat(40)),
                _ => format!("line {line}\n"),
            })
            .collect::<String>();
        fs::write(root.join(&path), &source).unwrap();
        run_git(&["add", "source.txt"]);
        run_git(&["commit", "-qm", "Add source lines"]);
        let oid = run_git(&["rev-parse", "HEAD"]).trim().to_owned();

        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1150.), px(760.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root,
                        file: Some(path.clone()),
                    },
                    window,
                    cx,
                );
            })
        });
        visual.run_until_parked();
        let draw = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| window.draw(cx).clear(cx));
        };
        draw(visual);
        let first = visual
            .debug_bounds("editor-blame")
            .expect("current line blame is visible");
        let at = first.center();
        visual.simulate_mouse_move(at, None, Modifiers::default());
        visual.executor().advance_clock(Duration::from_millis(400));
        visual.simulate_mouse_move(at + point(px(4.), px(0.)), None, Modifiers::default());
        visual.executor().advance_clock(Duration::from_millis(400));
        draw(visual);
        assert!(visual.debug_bounds("editor-blame-details").is_none());
        visual.executor().advance_clock(Duration::from_millis(150));
        draw(visual);
        let details = visual
            .debug_bounds("editor-blame-details")
            .expect("idle pointer opens commit details");
        visual.simulate_mouse_move(details.center(), None, Modifiers::default());
        visual.executor().advance_clock(Duration::from_millis(350));
        draw(visual);
        assert!(
            visual.debug_bounds("editor-blame-details").is_some(),
            "moving into details keeps them open"
        );
        let copy = visual
            .debug_bounds("blame-copy")
            .expect("copy action")
            .center();
        visual.simulate_mouse_move(copy, None, Modifiers::default());
        visual.simulate_click(copy, Modifiers::default());
        visual.update(|_, cx| assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), oid));

        visual.update(|_, cx| {
            let editor = workspace.read(cx).files[&path].editor.clone();
            editor.update(cx, |editor, cx| {
                editor.set_selected_range_with_direction(7..7, false, cx)
            });
        });
        visual.run_until_parked();
        draw(visual);
        let second = visual
            .debug_bounds("editor-blame")
            .expect("cached adjacent line is visible");
        assert!(second.top() > first.top());
        assert!(visual.debug_bounds("editor-blame-details").is_none());
        let editor = visual.update(|_, cx| workspace.read(cx).files[&path].editor.clone());
        // Freeze the background reply so every edited frame exercises the pending state.
        let original = visual.update(|_, cx| {
            workspace.update(cx, |workspace, _| {
                let blame = &mut workspace.files.get_mut(&path).unwrap().git.blame;
                blame.pending = true;
                blame.shown.as_ref().unwrap().label.clone()
            })
        });
        for replacement in ["draft ", "draft!"] {
            visual.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.edit_ranges(&[(7..13, replacement.into())], window, cx);
                    editor.set_selected_range_with_direction(7..7, false, cx);
                });
            });
            draw(visual);
            assert!(
                visual.debug_bounds("editor-blame").is_some(),
                "each pending frame paints the previous label"
            );
            visual.update(|_, cx| {
                assert_eq!(
                    workspace.read(cx).files[&path]
                        .git
                        .blame
                        .shown
                        .as_ref()
                        .unwrap()
                        .label,
                    original
                );
            });
        }
        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_range_with_direction(14..14, false, cx)
            });
        });
        draw(visual);
        assert!(
            visual.debug_bounds("editor-blame").is_none(),
            "a different line never inherits the pending label"
        );
        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_range_with_direction(7..7, false, cx)
            });
            workspace.update(cx, |workspace, cx| {
                workspace.files.get_mut(&path).unwrap().git.blame.pending = false;
                workspace.refresh_editor_blame(window, cx);
            });
        });
        visual.run_until_parked();
        draw(visual);
        visual.update(|_, cx| {
            let blame = &workspace.read(cx).files[&path].git.blame;
            assert!(matches!(
                blame.mapping.as_ref().unwrap().1.line(1, 80),
                Line::Unsaved
            ));
            assert_eq!(
                blame.shown.as_ref().unwrap().label.as_str(),
                t!("git.blame_unsaved").into_owned()
            );
        });
        assert!(visual.debug_bounds("editor-blame").is_some());

        let baseline_offset = |line, visual: &mut VisualTestContext| {
            visual.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.set_cursor_position(lsp_types::Position::new(line, 0), window, cx)
                });
            });
            visual.run_until_parked();
            PAINTED_BASELINE.set(None);
            draw(visual);
            let hint = visual
                .debug_bounds("editor-blame")
                .expect("blame is visible");
            PAINTED_BASELINE.get().expect("hint painted in this frame") - hint.top()
        };
        let text_baseline = baseline_offset(9, visual);
        for line in [10, 11, 12] {
            assert!(
                (baseline_offset(line, visual) - text_baseline).abs() < px(0.05),
                "blame on line {line} uses the same baseline offset as the ordinary text row",
            );
        }

        let wrapped = visual.update(|_, cx| editor.read(cx).projected_wrap_rows(12));
        assert!(wrapped > 1);
        let assert_frame = |visual: &mut VisualTestContext| {
            PAINTED_BASELINE.set(None);
            draw(visual);
            let hint = visual
                .debug_bounds("editor-blame")
                .expect("blame is visible on the first frame after scrolling");
            let (space, baseline) =
                visual.update(|_, cx| editor.read(cx).cursor_line_end_geometry().unwrap());
            assert!((hint.left() - space.left() - px(16.)).abs() <= px(0.5));
            assert!((hint.top() - space.top()).abs() <= px(0.5));
            let painted = PAINTED_BASELINE.get().expect("hint painted in this frame");
            assert!((painted - baseline).abs() < px(0.05));
            hint.origin
        };
        let mut previous = assert_frame(visual);
        for y in [-13., -29., -42.] {
            visual.update(|_, cx| {
                editor.update(cx, |editor, cx| {
                    editor.set_scroll_offset(point(px(0.), px(y)), cx)
                });
            });
            let current = assert_frame(visual);
            assert!(current.y < previous.y);
            previous = current;
        }
        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_soft_wrap(false, window, cx);
                editor.set_cursor_position(lsp_types::Position::new(13, 0), window, cx);
            });
        });
        visual.run_until_parked();
        draw(visual);
        let mut previous = assert_frame(visual);
        for x in [-8., -16., -24.] {
            visual.update(|_, cx| {
                editor.update(cx, |editor, cx| {
                    editor.set_scroll_offset(point(px(x), px(-42.)), cx);
                });
            });
            let current = assert_frame(visual);
            assert!(current.x < previous.x);
            previous = current;
        }

        let editor = visual.update(|_, cx| workspace.read(cx).files[&path].editor.clone());
        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_range_with_direction(0..0, false, cx);
                editor.set_scroll_offset(point(px(0.), px(-650.)), cx);
            })
        });
        visual.run_until_parked();
        draw(visual);
        assert!(
            visual.debug_bounds("editor-blame").is_none(),
            "the anchor is outside the viewport"
        );
        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_offset(point(px(0.), px(0.)), cx);
            })
        });
        visual.run_until_parked();
        draw(visual);
        let open_details = |visual: &mut VisualTestContext| {
            let anchor = visual.debug_bounds("editor-blame").unwrap().center();
            visual.simulate_mouse_move(point(px(1.), px(1.)), None, Modifiers::default());
            visual.simulate_mouse_move(anchor, None, Modifiers::default());
            visual.executor().advance_clock(Duration::from_millis(550));
            draw(visual);
        };
        open_details(visual);
        let diff_link = visual
            .debug_bounds("blame-diff")
            .expect("diff action")
            .center();
        visual.simulate_mouse_move(diff_link, None, Modifiers::default());
        visual.simulate_click(diff_link, Modifiers::default());
        visual.run_until_parked();
        draw(visual);
        assert!(
            visual.debug_bounds("git-diff-editor").is_some(),
            "diff opens the matching commit review"
        );
        visual.update(|window, cx| {
            let panel = workspace.read(cx).git.as_ref().unwrap();
            assert_eq!(
                panel.read(cx).session(cx).unwrap().selection,
                Some(crate::session::GitSelection::Commit(oid, None)),
            );
            workspace.update(cx, |workspace, cx| {
                workspace.switch_view(WorkspaceView::Editor, window, cx)
            });
        });
        visual.run_until_parked();
        draw(visual);
        open_details(visual);
        let source_link = visual
            .debug_bounds("blame-source")
            .expect("source action")
            .center();
        visual.simulate_mouse_move(source_link, None, Modifiers::default());
        visual.simulate_click(source_link, Modifiers::default());
        visual.run_until_parked();
        draw(visual);
        assert!(
            visual.debug_bounds("git-snapshot").is_some(),
            "source opens a read-only commit snapshot"
        );
    }
}
