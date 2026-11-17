//! Measure the editor's CPU path in a real GPUI window.
use std::{
    env,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui::{prelude::*, *};
use gpui_component::{
    Root,
    input::{Editor, EditorState, RopeExt as _},
};
use stroll::{
    repository::{Document, Repository, Store},
    workspace::apply_theme,
};

struct EditorProbe {
    editor: Entity<EditorState>,
    width: Pixels,
}

impl Render for EditorProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(self.width).h_full().child(
            Editor::new(&self.editor)
                .appearance(false)
                .bordered(false)
                .size_full(),
        )
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [root, file, language] = args.as_slice() else {
        anyhow::bail!("usage: editor_perf REPOSITORY FILE LANGUAGE");
    };
    let repository = Repository::open(Path::new(root))?;
    let file = PathBuf::from(file);
    let language = language.to_owned();
    let state_dir = tempfile::tempdir()?;
    let store = Store::new(state_dir.path().to_owned())?;

    gpui_platform::application()
        .with_assets(gpui_kit_assets::AllAssets)
        .run(move |cx| {
            gpui_component::init(cx);
            apply_theme(gpui_component::Theme::global(cx).mode, cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(850.)),
                    cx,
                ))),
                ..Default::default()
            };
            let window = cx
                .open_window(options, |window, cx| {
                    let editor = cx.new(|cx| EditorState::new(window, cx));
                    let probe = cx.new(|_| EditorProbe {
                        editor,
                        width: px(1010.),
                    });
                    cx.new(|cx| Root::new(probe, window, cx))
                })
                .expect("open performance probe");
            let window = AnyWindowHandle::from(window);
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_millis(350)).await;
                for sample in 0..3 {
                    let start = Instant::now();
                    let doc = Document::open(&repository, &file, &store).expect("load sample");
                    let read = start.elapsed();
                    let (probe, editor, first_draw) = window
                        .update(cx, |root, window, cx| {
                            let probe = root
                                .downcast::<Root>()
                                .unwrap()
                                .read(cx)
                                .view()
                                .clone()
                                .downcast::<EditorProbe>()
                                .unwrap();
                            let editor = cx.new(|cx| {
                                EditorState::new(window, cx)
                                    .progressive_wrap()
                                    .language(language.clone())
                                    .line_number(true)
                                    .default_value(doc.text)
                            });
                            probe.update(cx, |probe, cx| {
                                probe.editor = editor.clone();
                                probe.width = px(1010.);
                                cx.notify();
                            });
                            editor.update(cx, |editor, cx| editor.focus(window, cx));
                            let before_draw = Instant::now();
                            window.draw(cx).clear(cx);
                            (probe, editor, before_draw.elapsed())
                        })
                        .expect("draw new buffer");
                    let first_editable = start.elapsed();
                    let wrap_done = await_wrap(&editor, cx, start).await;
                    cx.background_executor().timer(Duration::from_millis(120)).await;
                    let scroll = measure_frames(window, &editor, cx, false).await;
                    let input = measure_frames(window, &editor, cx, true).await;
                    let resize = window.update(cx, |_, window, cx| {
                        let start = Instant::now();
                        probe.update(cx, |probe, cx| {
                            probe.width = px(820.);
                            cx.notify();
                        });
                        window.draw(cx).clear(cx);
                        start.elapsed()
                    }).unwrap();
                    println!(
                        "sample={} read={:.2} first_draw={:.2} first_editable={:.2} wrap_done={} scroll_p95={:.2} input_p95={:.2} resize_draw={:.2}",
                        sample + 1, ms(read), ms(first_draw), ms(first_editable),
                        wrap_done.map_or_else(|| "timeout".to_owned(), |elapsed| format!("{:.2}", ms(elapsed))),
                        ms(scroll), ms(input), ms(resize),
                    );
                }
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    Ok(())
}

async fn await_wrap(
    editor: &Entity<EditorState>,
    cx: &mut AsyncApp,
    start: Instant,
) -> Option<Duration> {
    while start.elapsed() < Duration::from_secs(30) {
        if !editor.read_with(cx, |editor, _| editor.is_wrapping()) {
            return Some(start.elapsed());
        }
        cx.background_executor()
            .timer(Duration::from_millis(12))
            .await;
    }
    None
}

async fn measure_frames(
    handle: AnyWindowHandle,
    editor: &Entity<EditorState>,
    cx: &mut AsyncApp,
    edit: bool,
) -> Duration {
    handle
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_offset(point(px(0.), px(0.)), cx);
            });
            window.draw(cx).clear(cx);
        })
        .unwrap();

    let mut samples = Vec::new();
    for frame in 0..48 {
        let elapsed = handle
            .update(cx, |_, window, cx| {
                let start = Instant::now();
                editor.update(cx, |editor, cx| {
                    if edit {
                        editor.replace_text_in_range(None, "a", window, cx);
                    } else {
                        let lines = editor.text().lines_len().min(120);
                        let step = (frame * 3) % lines;
                        let offset = editor.line_height().unwrap_or(px(20.)) * step;
                        editor.set_scroll_offset(point(px(0.), -offset), cx);
                    }
                });
                window.draw(cx).clear(cx);
                start.elapsed()
            })
            .expect("draw editor interaction");
        samples.push(elapsed);
        cx.background_executor()
            .timer(Duration::from_millis(16))
            .await;
    }
    samples.sort_unstable();
    samples[samples.len() * 95 / 100]
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.
}
