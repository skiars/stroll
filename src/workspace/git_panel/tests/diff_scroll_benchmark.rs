use super::*;
use std::{cell::Cell, time::Instant};

const IMAGE_SECTION: &str = "images/";
const IMAGE_COUNT: u8 = 72;
const IMAGE_START: &str = "images/006.png";
const LARGE_IMAGE: &str = "images/032.jpg";
const AFTER_LARGE_IMAGE: &str = "images/033.png";
const DENSE_TEXT: &str = "text/dense.txt";
const WINDOW_SIZE: Size<Pixels> = size(px(1180.), px(780.));

struct Benchmark {
    root: PathBuf,
    commit: String,
    temp: tempfile::TempDir,
}

impl Benchmark {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(IMAGE_SECTION))?;
        fs::create_dir(root.join("text"))?;
        git(&root, &["init", "-qb", "main"]);
        git(&root, &["config", "user.name", "Benchmark"]);
        git(
            &root,
            &["config", "user.email", "benchmark@example.invalid"],
        );
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "--allow-empty", "-qm", "Start"]);
        for index in 0..IMAGE_COUNT {
            let (width, height, path) = if index == 32 {
                (4608, 6144, root.join(LARGE_IMAGE))
            } else {
                (
                    640,
                    448,
                    root.join(format!("{IMAGE_SECTION}{index:03}.png")),
                )
            };
            image::RgbImage::from_fn(width, height, |x, y| {
                image::Rgb([
                    (x as u8).wrapping_add(index),
                    (y as u8).wrapping_mul(3),
                    (x ^ y) as u8,
                ])
            })
            .save(path)?;
        }
        let text: String = (0..640)
            .map(|line| format!("{line:04} The quick brown fox jumps over the lazy dog.\n"))
            .collect();
        fs::write(root.join(DENSE_TEXT), text)?;
        git(&root, &["add", "--force", "."]);
        git(&root, &["commit", "-qm", "Add benchmark fixtures"]);
        let head = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "HEAD"])
            .output()?;
        anyhow::ensure!(
            head.status.success(),
            "Could not resolve the fixture commit"
        );
        let commit = String::from_utf8(head.stdout)?.trim_end().to_owned();
        Ok(Self { root, commit, temp })
    }

    fn workspace(&self, window: &mut Window, cx: &mut App) -> Entity<Workspace> {
        let store = Arc::new(Store::new(self.temp.path().join("state")).unwrap());
        let config = crate::config::ConfigFile::load(Some(self.temp.path().join("config.toml")));
        let workspace =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Git, window, cx));
        let root = self.root.clone();
        let view = workspace.clone();
        window.defer(cx, move |window, cx| {
            view.update(cx, |this, cx| {
                this.open_repository(Location { root, file: None }, window, cx)
            });
        });
        workspace
    }
}

fn select_commit(panel: &Entity<GitPanel>, commit: &str, window: &mut Window, cx: &mut App) {
    panel.update(cx, |panel, cx| {
        panel.split = false;
        panel.show_history = true;
        panel.show_tree = true;
        panel.select_history_target(Some(commit), window, cx);
        cx.notify();
    });
}

fn jump(panel: &Entity<GitPanel>, section: &str, cx: &mut App) -> SharedString {
    let panel = panel.read(cx);
    let review = panel.review.as_ref().unwrap();
    assert_eq!(review.detail.files.len(), usize::from(IMAGE_COUNT) + 1);
    let path = review
        .detail
        .files
        .iter()
        .find(|file| file.path.starts_with(section))
        .unwrap()
        .path
        .clone();
    panel
        .diff
        .clone()
        .update(cx, |diff, cx| diff.scroll_to_file(&path, cx));
    path.into()
}

fn scroll(window: &mut Window, cx: &mut App, distance: f32) {
    window.dispatch_event(
        PlatformInput::Scroll(ScrollEvent {
            position: point(px(740.), px(400.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(distance))),
            source: ScrollSource::Pointer,
            touch_phase: TouchPhase::Moved,
            modifiers: Modifiers::default(),
        }),
        cx,
    );
}

fn snapshot(panel: &Entity<GitPanel>, cx: &App) -> (usize, usize, Option<SharedString>) {
    let panel = panel.read(cx);
    (
        panel.diff.read(cx).editor_row_revision(),
        panel.review.as_ref().unwrap().images.len(),
        panel.diff.read(cx).top_file().cloned(),
    )
}

struct BenchmarkRenderer {
    gpu: gpui_wgpu::WgpuHeadlessRenderer,
    submit_ms: Rc<Cell<f64>>,
}

impl PlatformHeadlessRenderer for BenchmarkRenderer {
    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> Result<image::RgbaImage> {
        self.render_scene(scene, size)?;
        Ok(image::RgbaImage::new(0, 0))
    }

    fn render_scene(&mut self, scene: &Scene, size: Size<DevicePixels>) -> Result<()> {
        let start = Instant::now();
        self.gpu.render_scene(scene, size)?;
        self.submit_ms.set(start.elapsed().as_secs_f64() * 1000.);
        Ok(())
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.gpu.sprite_atlas()
    }
}

fn percentile(samples: &mut [f64], percentile: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[((samples.len() as f64 * percentile).ceil() as usize).saturating_sub(1)]
}

#[::core::prelude::v1::test]
#[ignore = "manual release-mode benchmark with generated Git fixtures"]
fn git_image_diff_scroll_budget() -> Result<()> {
    let benchmark = Benchmark::new()?;
    let submit = Rc::new(Cell::new(0.));
    let submit_ms = submit.clone();
    let gpu = std::env::var_os("STROLL_DIFF_BENCH_GPU").is_some_and(|value| value == "1");
    let mut app = HeadlessAppContext::with_platform(
        crate::test_support::text_system(true),
        Arc::new(gpui_kit_assets::AllAssets),
        move || {
            if !gpu {
                return Ok(None);
            }
            Ok(Some(Box::new(BenchmarkRenderer {
                gpu: gpui_wgpu::WgpuHeadlessRenderer::new()?,
                submit_ms: submit_ms.clone(),
            })))
        },
    );
    app.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let mut workspace = None;
    let handle = app.open_window(WINDOW_SIZE, |window, cx| {
        let view = benchmark.workspace(window, cx);
        workspace = Some(view.clone());
        cx.new(|cx| gpui_component::Root::new(view, window, cx))
    })?;
    let workspace = workspace.unwrap();
    let draw = |app: &mut HeadlessAppContext| {
        app.update_window(handle.into(), |_, window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
        })
    };
    let settle = |app: &mut HeadlessAppContext| -> Result<()> {
        for _ in 0..5 {
            app.run_until_parked();
            draw(app)?;
        }
        Ok(())
    };
    settle(&mut app)?;
    let panel = app.update_window(handle.into(), |_, window, cx| {
        window.activate_window();
        let panel = workspace.read(cx).git.clone().unwrap();
        select_commit(&panel, &benchmark.commit, window, cx);
        panel
    })?;
    settle(&mut app)?;
    for label in ["cold", "warm", "reverse", "split", "text"] {
        if matches!(label, "split" | "text") {
            app.update(|cx| {
                panel.update(cx, |panel, cx| {
                    panel.split = label == "split";
                    cx.notify();
                })
            });
            settle(&mut app)?;
            let mode = if label == "split" {
                DiffMode::Split
            } else {
                DiffMode::Unified
            };
            assert_eq!(app.update(|cx| panel.read(cx).diff.read(cx).mode()), mode);
        }
        let section = if label == "text" {
            DENSE_TEXT
        } else {
            IMAGE_SECTION
        };
        let path = app.update(|cx| jump(&panel, section, cx));
        settle(&mut app)?;
        let before = app.update(|cx| snapshot(&panel, cx));
        assert_eq!(before.2.as_ref(), Some(&path));
        let mut cpu = Vec::new();
        let mut total = Vec::new();
        for frame in 0..155 {
            app.advance_clock(Duration::from_millis(17));
            let cpu_ms = app.update_window(handle.into(), |_, window, cx| {
                let start = Instant::now();
                scroll(
                    window,
                    cx,
                    if label == "reverse" && frame >= 100 {
                        28.
                    } else {
                        -24.
                    },
                );
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
                let cpu = start.elapsed().as_secs_f64() * 1000.;
                if gpu {
                    window.render_to_image().unwrap();
                }
                cpu
            })?;
            cpu.push(cpu_ms);
            total.push(cpu_ms + submit.get());
            app.run_until_parked();
        }
        let after = app.update(|cx| snapshot(&panel, cx));
        if label != "text" {
            assert_ne!(before.2, after.2, "scroll must move the diff");
        }
        eprintln!(
            "{label} cpu_p50_ms={:.2} cpu_p95_ms={:.2} cpu_submit_p95_ms={:.2} row_revisions={} images={}..{} last={:?}",
            percentile(&mut cpu, 0.50),
            percentile(&mut cpu, 0.95),
            percentile(&mut total, 0.95),
            after.0 - before.0,
            before.1,
            after.1,
            after.2,
        );
    }
    Ok(())
}

const PRESENTATION_FRAMES: usize = 660;

fn replay_frames(
    label: &'static str,
    frame: usize,
    done: async_channel::Sender<()>,
    window: &Window,
) {
    window.on_next_frame(move |window, cx| {
        let (distance, frames) = if label == "large" {
            (if frame < 12 { -48. } else { 48. }, 22)
        } else if label == "reverse" {
            let delta = if (frame / 100).is_multiple_of(2) {
                -24.
            } else {
                24.
            };
            (delta, PRESENTATION_FRAMES)
        } else {
            (-10.5, PRESENTATION_FRAMES)
        };
        scroll(window, cx, distance);
        if frame + 1 < frames {
            replay_frames(label, frame + 1, done, window);
        } else {
            _ = done.try_send(());
        }
    });
}

#[::core::prelude::v1::test]
#[ignore = "manual Wayland compositor benchmark with generated Git fixtures"]
fn git_image_diff_wayland_presentation() -> Result<()> {
    anyhow::ensure!(
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        "Wayland is required"
    );
    anyhow::ensure!(
        std::env::var_os("STROLL_FRAME_TIMINGS").is_some_and(|value| value == "1"),
        "Set STROLL_FRAME_TIMINGS=1 to collect presentation feedback"
    );
    let benchmark = Benchmark::new()?;
    let (send, receive) = std::sync::mpsc::channel();
    gpui_platform::application()
        .with_assets(gpui_kit_assets::AllAssets)
        .run(move |cx| {
            gpui_component::init(cx);
            cx.activate(true);
            let mut workspace = None;
            let handle = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(0.), px(0.)),
                            WINDOW_SIZE,
                        ))),
                        ..Default::default()
                    },
                    |window, cx| {
                        let view = benchmark.workspace(window, cx);
                        workspace = Some(view.clone());
                        cx.new(|cx| gpui_component::Root::new(view, window, cx))
                    },
                )
                .unwrap();
            let workspace = workspace.unwrap();
            cx.spawn(async move |cx| {
                let Benchmark { temp: _temp, commit, .. } = benchmark;
                let commit = commit.as_str();
                let executor = cx.background_executor().clone();
                let result = async {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    let panel = loop {
                        let panel = cx.update(|cx| workspace.read(cx).git.clone());
                        if let Some(panel) = panel {
                            break panel;
                        }
                        anyhow::ensure!(Instant::now() < deadline, "Repository did not open");
                        executor.timer(Duration::from_millis(20)).await;
                    };
                    handle.update(cx, |_, window, cx| select_commit(&panel, commit, window, cx))?;
                    loop {
                        if cx.update(|cx| {
                            panel.read(cx).review.as_ref().is_some_and(|review| {
                                review.detail.comparison.commit_target() == Some(commit)
                            })
                        }) {
                            break;
                        }
                        anyhow::ensure!(Instant::now() < deadline, "Commit did not open");
                        executor.timer(Duration::from_millis(20)).await;
                    }
                    for label in ["cold", "warm", "reverse", "split"] {
                        if label == "split" {
                            cx.update(|cx| {
                                panel.update(cx, |panel, cx| {
                                    panel.split = true;
                                    cx.notify();
                                })
                            });
                            executor.timer(Duration::from_millis(200)).await;
                            anyhow::ensure!(
                                cx.update(|cx| panel.read(cx).diff.read(cx).mode()) == DiffMode::Split,
                                "Split mode requires a wider compositor output"
                            );
                        }
                        handle.update(cx, |_, window, cx| {
                            panel.update(cx, |panel, cx| {
                                panel.reveal_tree_file(IMAGE_START, window, cx);
                            });
                        })?;
                        let deadline = Instant::now() + Duration::from_secs(3);
                        let before = loop {
                            executor.timer(Duration::from_millis(40)).await;
                            let state = cx.update(|cx| {
                                (snapshot(&panel, cx), panel.read(cx).pending_file_reveal.is_none())
                            });
                            if state.0.2.as_deref() == Some(IMAGE_START) && state.1 {
                                break state.0;
                            }
                            anyhow::ensure!(Instant::now() < deadline, "Replay start did not settle: {state:?}");
                        };
                        let (done, finished) = async_channel::bounded(1);
                        let start = Instant::now();
                        handle.update(cx, |_, window, _| {
                            eprintln!(
                                "diff_scroll_begin case={label} active={} scale={}",
                                window.is_window_active(),
                                window.scale_factor()
                            );
                            replay_frames(label, 0, done, window);
                        })?;
                        futures_lite::future::race(
                            async { finished.recv().await.map_err(anyhow::Error::from) },
                            async {
                                executor.timer(Duration::from_secs(30)).await;
                                anyhow::bail!("Timed out waiting for compositor frames")
                            },
                        )
                        .await?;
                        let elapsed = start.elapsed().as_secs_f64();
                        let after = cx.update(|cx| snapshot(&panel, cx));
                        eprintln!(
                            "diff_scroll_end case={label} seconds={elapsed:.3} row_revisions={} images={}..{} last={:?}",
                            after.0 - before.0,
                            before.1,
                            after.1,
                            after.2
                        );
                        anyhow::ensure!(after.2 != before.2, "Replay did not move the diff");
                    }
                    // Reload at the largest JPEG, pass it quickly, then reverse and let scrolling settle.
                    let previous = cx.update(|cx| panel.read(cx).review.as_ref().unwrap().detail.clone());
                    handle.update(cx, |_, window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.load_review_at(
                                Selection::Commit(commit.to_owned(), None),
                                true,
                                Some(DiffPosition {
                                    path: LARGE_IMAGE.to_owned(),
                                    line: None,
                                    inset: 0.,
                                    horizontal: 0.,
                                }),
                                false,
                                window,
                                cx,
                            );
                        });
                    })?;
                    let start = Instant::now();
                    loop {
                        if cx.update(|cx| {
                            let panel = panel.read(cx);
                            panel.review.as_ref().is_some_and(|review| !Arc::ptr_eq(&review.detail, &previous))
                                && panel.diff.read(cx).top_file().is_some_and(|path| path.as_str() == LARGE_IMAGE)
                        }) {
                            break;
                        }
                        anyhow::ensure!(start.elapsed() < Duration::from_secs(15), "Large image range did not open");
                        executor.timer(Duration::from_millis(20)).await;
                    }
                    let (done, finished) = async_channel::bounded(1);
                    handle.update(cx, |_, window, _| replay_frames("large", 0, done, window))?;
                    finished.recv().await?;
                    executor.timer(Duration::from_millis(450)).await;
                    let viewport = cx.update(|cx| panel.read(cx).diff.read(cx).editor_viewport_position());
                    let stopped = Instant::now();
                    let mut ready = [None, None];
                    while ready.iter().any(Option::is_none) {
                        cx.update(|cx| {
                            let review = panel.read(cx).review.as_ref().unwrap();
                            for (path, time) in [LARGE_IMAGE, AFTER_LARGE_IMAGE].into_iter().zip(&mut ready) {
                                if matches!(review.images.get(&review.by_path[path]), Some([_, review::DiffImage::Ready(_)])) {
                                    time.get_or_insert_with(|| stopped.elapsed().as_millis());
                                }
                            }
                        });
                        anyhow::ensure!(stopped.elapsed() < Duration::from_secs(10), "Large image thumbnails did not finish");
                        executor.timer(Duration::from_millis(20)).await;
                    }
                    anyhow::ensure!(
                        viewport == cx.update(|cx| panel.read(cx).diff.read(cx).editor_viewport_position()),
                        "Thumbnail completion changed the stopped viewport"
                    );
                    eprintln!("diff_scroll_large wait_after_settle_ms={ready:?}");
                    Ok(())
                }
                .await;
                send.send(result).unwrap();
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    receive.recv()?
}
