use std::{
    cell::{Cell, RefCell},
    time::Instant,
};

use gpui::InputEvent as _;
use gpui_component::{Root, ThemeMode};
use gpui_wgpu::WgpuHeadlessRenderer;
use image::RgbaImage;

use super::*;

const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(1040.),
    height: px(720.),
};
const NARROW_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(820.),
    height: px(680.),
};

type PaintedPaths = Rc<RefCell<Vec<gpui::Path<ScaledPixels>>>>;

struct ScaledRenderer {
    renderer: WgpuHeadlessRenderer,
    scale: Rc<Cell<f32>>,
    window_size: Size<Pixels>,
    capture_paths: Option<PaintedPaths>,
}

impl PlatformHeadlessRenderer for ScaledRenderer {
    fn render_scene_to_image(&mut self, scene: &Scene, _: Size<DevicePixels>) -> Result<RgbaImage> {
        if let Some(paths) = &self.capture_paths {
            paths.replace(scene.paths.clone());
        }
        // The test scale override changes scene geometry, so match the target to it.
        self.renderer
            .render_scene_to_image(scene, self.window_size.to_device_pixels(self.scale.get()))
    }

    fn render_scene(&mut self, scene: &Scene, _: Size<DevicePixels>) -> Result<()> {
        self.renderer
            .render_scene(scene, self.window_size.to_device_pixels(self.scale.get()))
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.renderer.sprite_atlas()
    }
}

fn create_history(root: &Path, message_body: Option<&str>) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Reviewer"]);
    git(root, &["config", "user.email", "reviewer@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    let mut timestamp = 1_700_000_000;
    let mut commit_tree = |args: &[&str]| {
        timestamp += 60;
        let date = format!("@{timestamp} +0000");
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let tree = commit_tree(&["write-tree"]);
    let mut commit = |subject: &str, parents: &[&str]| {
        let mut args = vec!["commit-tree", tree.as_str(), "-m", subject];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        if let Some(body) = message_body {
            args.extend(["-m", body]);
        }
        commit_tree(&args)
    };
    let mut main = commit("Initial commit", &[]);
    for ix in 1..=12 {
        let base = main;
        let side = commit("Add regression coverage", &[&base]);
        let side = commit("Verify merge edge cases", &[&side]);
        let subject = if ix == 11 {
            "Refine repository history labels and preserve scroll position for unusually long titles that need to wrap and then show an ellipsis after their second line"
        } else {
            "Refine repository history labels"
        };
        main = commit(subject, &[&base]);
        main = commit("Keep scroll position stable", &[&main]);
        let subject = format!("Merge topic {ix}");
        main = commit(&subject, &[&main, &side]);
        if ix == 12 {
            git(root, &["branch", "topic", &side]);
            git(root, &["branch", "review/long-running-topic", &main]);
            git(root, &["tag", "candidate-with-release-notes", &main]);
            git(
                root,
                &[
                    "update-ref",
                    "refs/remotes/origin/review/long-running-topic",
                    &main,
                ],
            );
        }
    }
    git(root, &["update-ref", "refs/heads/main", &main]);
}

fn track_profile(image: &RgbaImage, x: u32, y: u32) -> [u32; 8] {
    let background = image.get_pixel(x + 5, y);
    std::array::from_fn(|ix| {
        image.get_pixel(x - 4 + ix as u32, y).0[..3]
            .iter()
            .zip(&background.0[..3])
            .map(|(&a, &b)| u32::from(a.abs_diff(b)))
            .sum()
    })
}

fn assert_track_crosses_seam(image: &RgbaImage, x: u32, y: u32, factor: f32) {
    let contrast = |row| track_profile(image, x, row).into_iter().max().unwrap();
    let nearby = contrast(y - 4).min(contrast(y + 4));
    let seam = (y - 2..=y + 1).map(contrast).min().unwrap();
    assert!(
        nearby > 100 && seam * 100 >= nearby * 95,
        "track fades at {x}, {y} at {factor}x: seam contrast {seam}, nearby {nearby}"
    );
}

struct HistoryWithPointerProbe {
    workspace: Entity<Workspace>,
    hitbox: Rc<RefCell<Option<gpui::Hitbox>>>,
}

impl Render for HistoryWithPointerProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let hitbox = self.hitbox.clone();
        div().size_full().child(self.workspace.clone()).child(
            canvas(
                move |bounds, window, _| {
                    hitbox.replace(Some(window.insert_hitbox(bounds, HitboxBehavior::Normal)));
                },
                |_, (), _, _| {},
            )
            .absolute()
            .inset_0(),
        )
    }
}

fn open_history(
    root: &Path,
    state: &Path,
    capture_paths: Option<PaintedPaths>,
    pointer_probe: Option<Rc<RefCell<Option<gpui::Hitbox>>>>,
) -> Result<(
    HeadlessAppContext,
    WindowHandle<Root>,
    Rc<Cell<f32>>,
    Entity<GitPanel>,
)> {
    open_history_at_size(root, state, capture_paths, pointer_probe, WINDOW_SIZE)
}

fn open_history_at_size(
    root: &Path,
    state: &Path,
    capture_paths: Option<PaintedPaths>,
    pointer_probe: Option<Rc<RefCell<Option<gpui::Hitbox>>>>,
    window_size: Size<Pixels>,
) -> Result<(
    HeadlessAppContext,
    WindowHandle<Root>,
    Rc<Cell<f32>>,
    Entity<GitPanel>,
)> {
    let platform = gpui_platform::current_platform(true);
    let scale = Rc::new(Cell::new(1.));
    let renderer_scale = scale.clone();
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        move || {
            Ok(Some(Box::new(ScaledRenderer {
                renderer: WgpuHeadlessRenderer::new()?,
                scale: renderer_scale.clone(),
                window_size,
                capture_paths: capture_paths.clone(),
            })))
        },
    );
    cx.update(gpui_component::init);
    let store = Arc::new(Store::new(state.to_path_buf())?);
    let mut workspace = None;
    let handle = cx.open_window(window_size, |window, cx| {
        let view = cx.new(|cx| {
            Workspace::with_config(
                store,
                crate::config::ConfigFile::load(None),
                WorkspaceView::Git,
                window,
                cx,
            )
        });
        workspace = Some(view.clone());
        if let Some(hitbox) = pointer_probe {
            let content = cx.new(|_| HistoryWithPointerProbe {
                workspace: view,
                hitbox,
            });
            cx.new(|cx| Root::new(content, window, cx))
        } else {
            cx.new(|cx| Root::new(view, window, cx))
        }
    })?;
    let workspace = workspace.unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        cx.set_reduce_motion(true);
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root: root.to_owned(),
                    file: None,
                },
                window,
                cx,
            );
        });
    })?;
    for _ in 0..4 {
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    }
    let panel = cx.read_entity(&workspace, |workspace, _| workspace.git.clone().unwrap());
    Ok((cx, handle, scale, panel))
}

#[::core::prelude::v1::test]
fn diff_file_headers_render_theme_lighting_before_and_after_pinning() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path();
    git(root, &["init", "-qb", "main"]);
    git(root, &["config", "user.name", "Reviewer"]);
    git(root, &["config", "user.email", "reviewer@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    fs::write(root.join("a.txt"), "before\ncontext\n")?;
    fs::write(root.join("b.txt"), "before\n".repeat(80))?;
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "Initial"]);
    fs::write(root.join("a.txt"), "after\ncontext\n")?;
    fs::write(root.join("b.txt"), "after\n".repeat(80))?;

    let state = tempfile::tempdir()?;
    let (mut cx, handle, _, panel) = open_history(root, state.path(), None, None)?;
    let window = handle.into();
    let settle = |cx: &mut HeadlessAppContext| -> Result<()> {
        for _ in 0..4 {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))?;
        }
        Ok(())
    };
    let headers = |cx: &mut HeadlessAppContext, mode: ThemeMode| {
        let background = if mode.is_dark() {
            rgb(0x26272c).into()
        } else {
            Palette::for_mode(mode).elevated
        };
        cx.update_window(window, |_, window, _| {
            window
                .painted_quads()
                .into_iter()
                .filter(|quad| {
                    quad.bounds.size.width.0 > 600.
                        && quad.bounds.size.height.0 == 32.
                        && quad.background.as_solid() == Some(background)
                })
                .map(|quad| quad.bounds)
                .collect::<Vec<_>>()
        })
    };
    let reveal = |cx: &mut HeadlessAppContext, path| -> Result<()> {
        cx.update_window(window, |_, _, cx| {
            panel
                .read(cx)
                .diff
                .clone()
                .update(cx, |diff, cx| diff.scroll_to_file(path, cx));
        })?;
        settle(cx)
    };
    let viewport = |cx: &HeadlessAppContext| {
        cx.read_entity(&panel, |panel, cx| {
            panel.diff.read(cx).editor_viewport_position()
        })
    };
    let assert_lighting = |image: &RgbaImage, header: Bounds<ScaledPixels>, mode| {
        let x = |fraction| (header.left().0 + header.size.width.0 * fraction) as u32;
        let y = header.top().0 as u32;
        let face = image.get_pixel(x(0.20), y + 4).0;
        let other_end = image.get_pixel(x(0.97), y + 4).0;
        let shadow = image.get_pixel(x(0.83), y + 33).0;
        let below = image.get_pixel(x(0.83), y + 46).0;
        assert!(shadow[0] < below[0], "{mode:?}: {shadow:?} vs {below:?}");
        if mode == ThemeMode::Light {
            let warmth = |pixel: [u8; 4]| i16::from(pixel[0]) - i16::from(pixel[2]);
            assert_eq!(other_end, [255; 4]);
            let warmest = image.get_pixel(x(0.) + 92, y + 4).0;
            let toward_right = image.get_pixel(x(0.55), y + 4).0;
            assert!(
                warmest[0] >= 251
                    && warmth(warmest) > warmth(face)
                    && warmth(face) > warmth(toward_right)
                    && warmth(toward_right) > warmth(other_end),
                "light highlight should fade into white: {warmest:?}, {face:?}, {toward_right:?}"
            );
            let halo = image.get_pixel(x(0.50), y + 33).0;
            assert!(
                warmth(halo) > warmth(shadow),
                "light halo should reach the middle of the header: {halo:?} vs {shadow:?}"
            );
        } else {
            assert_eq!(other_end, [38, 39, 44, 255]);
            let warmest = image.get_pixel(x(0.) + 72, y + 4).0;
            let toward_right = image.get_pixel(x(0.56), y + 4).0;
            assert_eq!(image.get_pixel(x(0.72), y + 4).0, other_end);
            assert!(
                warmest[0] > warmest[2]
                    && warmest[0] > face[0]
                    && face[0] > toward_right[0]
                    && toward_right[0] > other_end[0],
                "dark highlight should fade from the left: {warmest:?}, {face:?}, {toward_right:?}, {other_end:?}"
            );
        }
    };

    for (split, mode) in [(false, ThemeMode::Light), (true, ThemeMode::Dark)] {
        cx.update_window(window, |_, window, cx| {
            crate::workspace::apply_theme(mode, cx);
            window.set_scale_factor(1.);
            panel.update(cx, |panel, cx| {
                panel.show_history = false;
                panel.show_tree = false;
                panel.split = split;
                panel.select_worktree(window, cx);
                cx.notify();
            });
        })?;
        settle(&mut cx)?;
        reveal(&mut cx, "a.txt")?;
        let mut visible = headers(&mut cx, mode)?;
        visible.sort_by(|a, b| a.top().0.total_cmp(&b.top().0));
        assert_eq!(visible.len(), 2, "{mode:?}: {visible:?}");
        let in_flow = cx.capture_screenshot(window)?;
        assert_lighting(&in_flow, visible[1], mode);
        if mode.is_dark() {
            // Sample over the neutral hunk header before scrolling it out of view.
            let x = (visible[1].left().0 + visible[1].size.width.0 * 0.20) as u32;
            let y = visible[1].top().0 as u32 + 33;
            let shadow = in_flow.get_pixel(x, y).0;
            assert!(shadow[0] <= shadow[2], "dark shadow: {shadow:?}");
        }

        reveal(&mut cx, "b.txt")?;
        let aligned = headers(&mut cx, mode)?.pop().unwrap();
        let aligned_image = cx.capture_screenshot(window)?;
        let before_scroll = viewport(&cx);
        let scroll = |cx: &mut HeadlessAppContext, dy| -> Result<()> {
            cx.update_window(window, |_, window, cx| {
                window.dispatch_event(
                    ScrollEvent {
                        position: point(px(aligned.left().0 + 100.), px(aligned.bottom().0 + 40.)),
                        delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
                        touch_phase: TouchPhase::Moved,
                        source: ScrollSource::Pointer,
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            })?;
            settle(cx)
        };
        scroll(&mut cx, 16.)?;
        assert!(viewport(&cx) < before_scroll);
        let mut moving_headers = headers(&mut cx, mode)?;
        moving_headers.sort_by(|a, b| a.top().0.total_cmp(&b.top().0));
        assert_eq!(moving_headers.len(), 2, "{mode:?}: {moving_headers:?}");
        let [outgoing, approaching] = [moving_headers[0], moving_headers[1]];
        assert_eq!(outgoing.top().0, aligned.top().0 - 16.);
        assert_eq!(approaching.top().0, aligned.top().0 + 16.);
        let halfway = cx.capture_screenshot(window)?;
        scroll(&mut cx, -40.)?;
        let pinned = headers(&mut cx, mode)?.pop().unwrap();
        assert_eq!(pinned, aligned);
        let pinned_image = cx.capture_screenshot(window)?;
        assert_lighting(&pinned_image, pinned, mode);
        let x = pinned.left().0 as u32;
        let y = pinned.top().0 as u32;
        for dy in [4, 16, 28] {
            assert_eq!(
                aligned_image.get_pixel(x + 200, y + dy),
                pinned_image.get_pixel(x + 200, y + dy),
                "{mode:?}: pinning should leave the title lighting unchanged"
            );
        }
        let (regular_ink, halfway_ink, pinned_ink) = if mode.is_dark() {
            ([220, 221, 226], [222, 218, 214], [224, 215, 202])
        } else {
            ([48, 53, 64], [75, 61, 47], [102, 69, 30])
        };
        let has_ink = |image: &RgbaImage, header: Bounds<ScaledPixels>, expected: [u8; 3]| {
            let header_y = header.top().0 as u32;
            ((header_y + 8).max(y)..header_y + 26).any(|row| {
                (x + 48..x + 84).any(|column| {
                    image
                        .get_pixel(column, row)
                        .0
                        .into_iter()
                        .zip(expected)
                        .all(|(channel, expected)| channel.abs_diff(expected) <= 1)
                })
            })
        };
        assert!(
            has_ink(&in_flow, visible[1], regular_ink),
            "{mode:?}: in-flow filename"
        );
        assert!(
            has_ink(&halfway, approaching, halfway_ink),
            "{mode:?}: approaching filename"
        );
        assert!(
            has_ink(&halfway, outgoing, halfway_ink),
            "{mode:?}: outgoing filename"
        );
        assert!(
            has_ink(&aligned_image, aligned, pinned_ink),
            "{mode:?}: filename at the viewport edge"
        );
        assert!(
            has_ink(&pinned_image, pinned, pinned_ink),
            "{mode:?}: pinned filename"
        );
        if let Some(output) = std::env::var_os("STROLL_GIT_SCREENSHOTS") {
            let output = PathBuf::from(output);
            fs::create_dir_all(&output)?;
            in_flow.save(output.join(format!("diff-headers-{mode:?}.png")))?;
            pinned_image.save(output.join(format!("diff-header-pinned-{mode:?}.png")))?;
        }
    }
    Ok(())
}

#[::core::prelude::v1::test]
fn compares_outside_the_selected_branch_from_both_toolbar_entries() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path();
    create_history(root, None);
    git(root, &["switch", "-qc", "other-version"]);
    fs::write(root.join("code.txt"), "other\n")?;
    git(root, &["add", "code.txt"]);
    git(
        root,
        &[
            "commit",
            "-qm",
            "Alternate change",
            "-m",
            "A reference for the picker",
        ],
    );
    let other = git::head(root)?.unwrap();
    git(root, &["switch", "-q", "main"]);
    fs::write(root.join("code.txt"), "main\n")?;
    git(root, &["add", "code.txt"]);
    git(root, &["commit", "-qm", "Main change"]);
    let main = git::head(root)?.unwrap();
    fs::write(root.join("code.txt"), "staged\n")?;
    git(root, &["add", "code.txt"]);
    fs::write(root.join("code.txt"), "worktree\n")?;
    let state = tempfile::tempdir()?;
    let store = Arc::new(Store::new(state.path().join("test"))?);
    let config = crate::config::ConfigFile::load(Some(state.path().join("config.toml")));
    let platform = gpui_platform::current_platform(true);
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    let mut workspace = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Git, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_scale_factor_change(1.);
    visual.simulate_resize(size(px(1280.), px(850.)));
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root: root.to_owned(),
                    file: None,
                },
                window,
                cx,
            );
        });
    });
    let draw = |visual: &mut VisualTestContext| {
        for _ in 0..2 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
    };
    let click_button = |visual: &mut VisualTestContext, selector| {
        let bounds = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing {selector}"));
        visual.simulate_click(bounds.center(), Modifiers::default());
        draw(visual);
    };
    let open_picker = |visual: &mut VisualTestContext, selector| {
        click_button(visual, selector);
        let dialog = visual.debug_bounds("dialog-0").unwrap().scale(1.);
        let trigger = visual.debug_bounds(selector).unwrap().scale(1.);
        visual.update(|window, _| {
            let quads = window.painted_quads();
            let dialog_order = quads
                .iter()
                .find(|quad| quad.bounds == dialog && !quad.background.is_transparent())
                .expect("dialog surface")
                .order;
            let popup_order = quads
                .iter()
                .find(|quad| {
                    quad.bounds.origin.x == trigger.origin.x
                        && quad.bounds.origin.y > trigger.bottom()
                        && quad.bounds.origin.y < dialog.bottom()
                        && quad.bounds.size.width == trigger.size.width
                        && quad.bounds.size.height > trigger.size.height
                        && !quad.background.is_transparent()
                })
                .expect("dropdown surface overlapping the dialog")
                .order;
            assert!(popup_order > dialog_order, "{selector} is under the dialog");
        });
    };
    let pick_base = |visual: &mut VisualTestContext, query| {
        open_picker(visual, "git-comparison-base");
        visual.simulate_input(query);
        for _ in 0..2 {
            visual.executor().advance_clock(Duration::from_millis(300));
            draw(visual);
        }
        visual.simulate_keystrokes("down enter");
        draw(visual);
        click_button(visual, "git-apply-comparison");
    };
    draw(visual);
    let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
    let history_state = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            (
                panel.commits_requests,
                panel.history_scroll.logical_scroll_top(),
                panel.history_scroll.bounds_for_item(1).map(|mut bounds| {
                    bounds.origin -= panel.history_scroll.viewport_bounds().origin;
                    bounds
                }),
            )
        })
    };
    let row = |visual: &mut VisualTestContext, index| {
        visual.update(|_, cx| {
            panel
                .read(cx)
                .history_scroll
                .bounds_for_item(index)
                .unwrap()
        })
    };
    let assert_highlight = |visual: &mut VisualTestContext, index, selected| {
        let bounds = row(visual, index).scale(1.);
        visual.update(|window, cx| {
            let mode = cx.theme().mode;
            let color = Palette::for_mode(mode)
                .elevated
                .blend(graph::main_color(mode).opacity(0.09));
            assert_eq!(
                window.painted_quads().iter().any(|quad| {
                    quad.bounds == bounds && quad.background.as_solid() == Some(color)
                }),
                selected,
                "history row {index} highlight"
            );
        });
    };
    let click_row = |visual: &mut VisualTestContext, index| {
        let point = row(visual, index).center();
        visual.simulate_click(point, Modifiers::default());
        draw(visual);
    };
    let compare_with_row = |visual: &mut VisualTestContext, index| {
        let point = row(visual, index).center();
        visual.simulate_mouse_down(point, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(point, MouseButton::Right, Modifiers::default());
        draw(visual);
        visual.simulate_keystrokes("down enter");
        draw(visual);
    };
    let initial_history = history_state(visual);
    assert_eq!(
        visual
            .debug_bounds("git-worktree-mode")
            .unwrap()
            .size
            .height,
        px(26.)
    );
    click_button(visual, "git-worktree-mode");
    visual.simulate_keystrokes("up enter");
    draw(visual);
    open_picker(visual, "git-comparison-target");
    let target = visual
        .debug_bounds("git-comparison-target")
        .unwrap()
        .center();
    let target_offset = visual.update(|window, _| target - window.viewport_size().center());
    visual.simulate_keystrokes("escape");
    draw(visual);
    pick_base(visual, "reference for the picker");
    assert_highlight(visual, 0, true);
    click_row(visual, 0);
    assert_eq!(history_state(visual), initial_history);
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        let detail = &panel.review.as_ref().unwrap().detail;
        assert_eq!(
            detail.comparison,
            Comparison::Versions {
                base: Some(other.clone()),
                target: None
            }
        );
        assert!(!panel.current_worktree());
        assert_eq!(detail.files[0].path, "code.txt");
    });
    let assert_diff = |visual: &mut VisualTestContext, expected| {
        draw(visual);
        visual.update(|_, cx| {
            let review = panel.read(cx).review.as_ref().unwrap();
            let lines = review.documents[0].patch_source_lines().collect::<Vec<_>>();
            assert_eq!(
                lines,
                [
                    (DiffSide::Original, 1, "other"),
                    (DiffSide::Modified, 1, expected)
                ]
            );
        });
    };
    assert_diff(visual, "worktree");
    fs::write(root.join("code.txt"), "latest saved worktree\n")?;
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.refresh_worktree(window, cx));
    });
    assert_diff(visual, "latest saved worktree");
    compare_with_row(visual, 1);
    assert_highlight(visual, 0, true);
    visual.update(|_, cx| {
        assert_eq!(
            panel.read(cx).review.as_ref().unwrap().detail.comparison,
            Comparison::Versions {
                base: Some(main.clone()),
                target: None,
            }
        );
    });
    assert_eq!(history_state(visual), initial_history);
    assert_eq!(
        visual
            .debug_bounds("git-custom-comparison")
            .unwrap()
            .size
            .height,
        px(26.)
    );
    click_button(visual, "git-custom-comparison");
    visual.simulate_keystrokes("up enter");
    draw(visual);
    visual.update(|_, cx| assert!(panel.read(cx).current_worktree()));

    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.history_scroll.scroll_to(ListOffset {
                item_ix: 1,
                offset_in_item: px(6.),
            });
            cx.notify();
        });
        window.refresh();
    });
    draw(visual);
    let scrolled_history = history_state(visual);
    assert_ne!(scrolled_history.1, initial_history.1);
    click_row(visual, 1);
    click_button(visual, "git-select-parent");
    visual.simulate_keystrokes("down enter");
    draw(visual);
    pick_base(visual, git::short(&other));
    assert_highlight(visual, 1, true);
    click_row(visual, 1);
    visual.update(|_, cx| {
        let detail = &panel.read(cx).review.as_ref().unwrap().detail;
        assert_eq!(
            detail.comparison,
            Comparison::Versions {
                base: Some(other),
                target: Some(main.clone())
            }
        );
    });
    assert_eq!(history_state(visual), scrolled_history);
    let parent = visual.update(|_, cx| panel.read(cx).commits[1].oid.to_string());
    compare_with_row(visual, 2);
    assert_highlight(visual, 1, true);
    assert_highlight(visual, 2, false);
    visual.update(|_, cx| {
        assert_eq!(
            panel.read(cx).review.as_ref().unwrap().detail.comparison,
            Comparison::Versions {
                base: Some(parent.clone()),
                target: Some(main.clone()),
            }
        );
    });
    assert_eq!(history_state(visual), scrolled_history);
    click_row(visual, 2);
    assert_highlight(visual, 2, true);
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(matches!(&panel.selection, Some(Selection::Commit(oid, None)) if oid == &parent));
    });
    assert_eq!(history_state(visual), scrolled_history);

    for scale in [1., 1.25] {
        visual.simulate_scale_factor_change(scale);
        draw(visual);
        let before = history_state(visual);
        let before_offset_ratio = before.1.offset_in_item / before.2.unwrap().size.height;
        let mut previous_width = px(0.);
        for size in [12., 13., 14.] {
            visual.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let mut draft = workspace.config.current().clone();
                    draft.appearance.ui_font_size = size;
                    workspace.commit_settings(draft, window, cx).unwrap();
                });
                assert_eq!(cx.theme().control_font_size, px(size as f32));
            });
            draw(visual);
            let after = history_state(visual);
            assert_eq!(after.0, before.0);
            assert_eq!(after.1.item_ix, before.1.item_ix);
            let after_offset_ratio = after.1.offset_in_item / after.2.unwrap().size.height;
            assert!((after_offset_ratio - before_offset_ratio).abs() < 0.001);
            visual.update(|window, cx| {
                assert_eq!(window.rem_size(), px(size as f32));
                let panel = panel.read(cx);
                assert!(
                    matches!(&panel.selection, Some(Selection::Commit(oid, None)) if oid == &parent)
                );
            });
            for selector in [
                "git-branch-picker",
                "git-commit-search",
                "git-file-filter",
                "git-select-parent",
            ] {
                let bounds = visual.debug_bounds(selector).unwrap();
                assert!(
                    (bounds.size.height - px(2. * size as f32)).abs() <= px(0.51 / scale),
                    "{selector} at {size}px and {scale}x"
                );
                visual.update(|window, _| {
                    let bounds = bounds.scale(scale);
                    assert!(
                        window.painted_quads().iter().any(|quad| {
                            (quad.bounds.size.height.0 - bounds.size.height.0).abs() <= 1.
                                && (quad.bounds.origin.y.0 - bounds.origin.y.0).abs() <= 1.
                                && (quad.bounds.origin.x.0 - bounds.origin.x.0).abs() <= 1.
                                && (quad.bounds.size.width.0 - bounds.size.width.0).abs() <= 1.
                                && (quad.border_widths.top.0 > 0.
                                    || !quad.background.is_transparent())
                        }),
                        "missing painted frame for {selector} at {scale}x"
                    );
                });
            }
            let width = visual.debug_bounds("git-select-parent").unwrap().size.width;
            assert!(
                width > previous_width,
                "comparison label at {size}px and {scale}x"
            );
            previous_width = width;
        }
    }
    visual.simulate_resize(NARROW_WINDOW_SIZE);
    draw(visual);
    let narrow_history = history_state(visual);
    let toolbar = visual.debug_bounds("git-toolbar").unwrap();
    visual.update(|window, cx| panel.update(cx, |panel, cx| panel.find(window, cx)));
    draw(visual);
    let field = visual
        .debug_bounds("find-query-field")
        .expect("editor search field");
    assert!(field.top() >= toolbar.bottom());
    assert!(field.left() >= toolbar.left() && field.right() <= toolbar.right());
    visual.update(|window, cx| {
        let scale = window.scale_factor();
        assert_eq!(field.size.height, window.rem_size() * 2.);
        let field = field.scale(scale);
        assert!(window.painted_quads().into_iter().any(|quad| {
            (quad.border_color == cx.theme().input || quad.border_color == cx.theme().ring)
                && quad.border_widths.top.0 > 0.
                && (quad.bounds.top() - field.top()).0.abs() <= 1.
                && (quad.bounds.size.height - field.size.height).0.abs() <= 1.
        }));
    });
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert_eq!(history_state(visual), narrow_history);
    click_button(visual, "git-select-parent");
    visual.simulate_keystrokes("up enter");
    draw(visual);
    for selector in ["git-comparison-base", "git-comparison-target"] {
        let bounds = visual.debug_bounds(selector).unwrap();
        assert!((bounds.size.height - px(28.)).abs() <= px(0.41));
    }

    click_button(visual, "git-comparison-target");
    let base = visual.debug_bounds("git-comparison-base").unwrap();
    let target = visual.debug_bounds("git-comparison-target").unwrap();
    let popup = visual.debug_bounds("select-popup-surface").unwrap();
    let search = visual.debug_bounds("popup-list-search").unwrap();
    // The separator is snapped to device pixels at fractional scaling.
    assert!((search.size.height - px(29.)).abs() <= px(0.5));
    assert!(popup.top() > target.bottom() && popup.top() > base.bottom());
    let viewport_bottom = visual.update(|window, _| window.viewport_size().height);
    assert!(popup.bottom() <= viewport_bottom - px(7.));

    visual.simulate_event(ScrollEvent {
        position: point(search.center().x, search.bottom() + px(12.)),
        delta: ScrollDelta::Pixels(point(px(0.), px(-94.))),
        ..Default::default()
    });
    draw(visual);
    assert_eq!(visual.debug_bounds("popup-list-search"), Some(search));
    assert_eq!(history_state(visual), narrow_history);

    let settle_search = |visual: &mut VisualTestContext| {
        for _ in 0..2 {
            visual.executor().advance_clock(Duration::from_millis(300));
            draw(visual);
        }
    };
    let assert_filtered_popup = |visual: &mut VisualTestContext| {
        settle_search(visual);
        let filtered = visual.debug_bounds("select-popup-surface").unwrap();
        assert_eq!(filtered.top(), popup.top());
        assert_eq!(visual.debug_bounds("popup-list-search"), Some(search));
        assert!(filtered.size.height < popup.size.height);
        assert_eq!(history_state(visual), narrow_history);
    };
    visual.simulate_input(&git::short(&main));
    assert_filtered_popup(visual);
    visual.simulate_keystrokes("ctrl-a");
    visual.simulate_input("no-such-revision-for-this-test");
    assert_filtered_popup(visual);
    visual.simulate_keystrokes("ctrl-a backspace");
    settle_search(visual);
    assert_eq!(visual.debug_bounds("select-popup-surface"), Some(popup));
    assert_eq!(visual.debug_bounds("popup-list-search"), Some(search));
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert_eq!(history_state(visual), narrow_history);
    visual.update(|_, cx| {
        assert!(
            matches!(&panel.read(cx).selection, Some(Selection::Commit(oid, None)) if oid == &parent)
        );
    });

    if let Some(path) = std::env::var_os("STROLL_COMPARISON_SCREENSHOT") {
        let (mut cx, handle, _, panel) = open_history_at_size(
            root,
            &state.path().join("screenshot"),
            None,
            None,
            NARROW_WINDOW_SIZE,
        )?;
        cx.update_window(handle.into(), |_, window, cx| {
            window.set_scale_factor(1.);
            panel.update(cx, |panel, cx| panel.open_revision_dialog(window, cx));
        })?;
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
        cx.update_window(handle.into(), |_, window, cx| {
            click(window, cx, window.viewport_size().center() + target_offset);
        })?;
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
        cx.capture_screenshot(handle.into())?.save(path)?;
    }
    Ok(())
}

#[::core::prelude::v1::test]
fn commit_history_pages_on_scroll_without_moving_the_anchor() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    create_history(fixture.path(), None);
    for ix in 0..171 {
        git(
            fixture.path(),
            &["commit", "--allow-empty", "-qm", &format!("Review {ix}")],
        );
    }
    let state = tempfile::tempdir()?;
    let (mut cx, handle, _, panel) = open_history(fixture.path(), state.path(), None, None)?;
    let counts = |cx: &mut HeadlessAppContext| {
        cx.read_entity(&panel, |panel, _| {
            (panel.commits.len(), panel.commits_requests)
        })
    };
    assert_eq!(counts(&mut cx), (60, 1));
    let viewport = cx.read_entity(&panel, |panel, _| panel.history_scroll.viewport_bounds());
    let wheel = |delta| {
        PlatformInput::Scroll(ScrollEvent {
            position: viewport.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        })
    };
    let mut timings = Vec::new();
    for _ in 0..80 {
        let start = Instant::now();
        cx.update_window(handle.into(), |_, window, cx| {
            window.dispatch_event(wheel(-20.), cx);
            window.draw(cx).clear(cx);
        })?;
        timings.push(start.elapsed());
    }
    cx.run_until_parked();
    assert_eq!(counts(&mut cx), (60, 1));

    let mut page_max = Duration::ZERO;
    for (page, expected) in [(2, 120), (3, 180), (4, 232)] {
        let (anchor, row_y) = cx.update_window(handle.into(), |_, window, cx| {
            let scroll = panel.read(cx).history_scroll.clone();
            if page == 3 {
                scroll
                    .set_offset_from_scrollbar(point(px(0.), -scroll.max_offset_for_scrollbar().y));
                window.refresh();
            } else {
                window.dispatch_event(wheel(-40_000.), cx);
            }
            let start = Instant::now();
            window.draw(cx).clear(cx);
            page_max = page_max.max(start.elapsed());
            let anchor = scroll.logical_scroll_top();
            (
                anchor,
                scroll.bounds_for_item(anchor.item_ix).unwrap().top(),
            )
        })?;
        cx.run_until_parked();
        let (after, after_y) = cx.update_window(handle.into(), |_, window, cx| {
            let start = Instant::now();
            window.draw(cx).clear(cx);
            page_max = page_max.max(start.elapsed());
            let scroll = &panel.read(cx).history_scroll;
            (
                scroll.logical_scroll_top(),
                scroll.bounds_for_item(anchor.item_ix).unwrap().top(),
            )
        })?;
        assert_eq!(counts(&mut cx), (expected, page));
        assert_eq!(after, anchor);
        assert_eq!(after_y, row_y);
    }
    cx.update_window(handle.into(), |_, window, cx| {
        window.dispatch_event(wheel(-40_000.), cx);
        window.draw(cx).clear(cx);
    })?;
    cx.run_until_parked();
    assert_eq!(counts(&mut cx), (232, 4));
    assert!(!cx.read_entity(&panel, |panel, _| panel.more));
    timings.sort_unstable();
    eprintln!(
        "History scroll input + draw p50={:.2} ms, p95={:.2} ms, page-boundary draw max={:.2} ms",
        timings[40].as_secs_f64() * 1000.,
        timings[76].as_secs_f64() * 1000.,
        page_max.as_secs_f64() * 1000.,
    );
    Ok(())
}

#[::core::prelude::v1::test]
fn commit_history_renders_at_multiple_scales() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    create_history(fixture.path(), None);
    let state = tempfile::tempdir()?;
    let (mut cx, handle, scale, panel) = open_history(fixture.path(), state.path(), None, None)?;
    let history = cx.update_window(handle.into(), |_, _, cx| {
        let panel = panel.read(cx);
        assert_eq!(panel.commits.len(), 60);
        panel.history_scroll.clone()
    })?;
    let output = std::env::var_os("STROLL_HISTORY_SCREENSHOTS").map(PathBuf::from);
    for factor in [1., 1.25, 1.5, 2.] {
        scale.set(factor);
        let (branch_x, seams) = cx.update_window(handle.into(), |_, window, cx| {
            window.set_scale_factor(factor);
            history.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.4),
            });
            window.draw(cx).clear(cx);
            let item = |ix| history.bounds_for_item(ix).unwrap();
            let tolerance = px(1. / factor);
            // Decorated rows are content-driven by the 1.4 line height, so
            // their height snaps to different device pixels per scale.
            let decorated_tolerance = px(2.);
            assert!((item(1).size.height - px(66.)).abs() <= decorated_tolerance);
            assert!((item(2).size.height - px(54.)).abs() <= tolerance);
            assert!((item(4).size.height - px(66.)).abs() <= decorated_tolerance);
            let branch_x = item(1).left() + window.rem_size() / 2. + px(20.);
            // The second lane leaves the first merge, runs straight through four rows,
            // then bends into the next merge.
            let seams = [1, 2, 3, 4, 5].map(|ix| item(ix).bottom());
            (branch_x, seams)
        })?;
        let image = cx.capture_screenshot(handle.into())?;
        assert_eq!(
            image.dimensions(),
            ((1040. * factor) as u32, (720. * factor) as u32)
        );
        if let Some(output) = &output {
            fs::create_dir_all(output)?;
            image.save(output.join(format!("history-{}.png", (factor * 100.) as u32)))?;
        }
        let x = (branch_x.as_f32() * factor).round() as u32;
        let seams = seams.map(|y| (y.as_f32() * factor).round() as u32);
        for y in seams {
            assert_track_crosses_seam(&image, x, y, factor);
        }
        if factor == 2. {
            let y = seams[0];
            let straight = track_profile(&image, x, y + 8);
            assert!((3..=5).contains(&straight.iter().filter(|&&pixel| pixel > 0).count()));
            // Check antialiasing coverage on both sides, including the curve's vertical lead.
            for row in [y - 8, y - 1, y, y + 1] {
                assert_eq!(track_profile(&image, x, row), straight);
            }
        }
    }
    if output.is_some() {
        cx.update_window(handle.into(), |_, window, cx| {
            window.set_scale_factor(1.25);
            window.draw(cx).clear(cx);
            let position = history.viewport_bounds().center();
            for _ in 0..5 {
                let mut frames = Vec::new();
                for ix in 0..160 {
                    let delta = if ix % 80 < 40 { -58. } else { 58. };
                    window.dispatch_event(
                        PlatformInput::Scroll(ScrollEvent {
                            position,
                            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Moved,
                            source: ScrollSource::Pointer,
                        }),
                        cx,
                    );
                    let start = Instant::now();
                    window.draw(cx).clear(cx);
                    frames.push(start.elapsed().as_secs_f64() * 1000.);
                    if ix == 39 {
                        assert!(history.logical_scroll_top().item_ix > 25);
                    }
                }
                frames.sort_by(f64::total_cmp);
                eprintln!(
                    "history draw p50={:.3} ms p95={:.3} ms",
                    frames[80], frames[151]
                );
            }
        })?;
    }
    let oid = cx.read_entity(&panel, |panel, _| panel.commits[1].oid.clone());
    let anchor = ListOffset {
        item_ix: 2,
        offset_in_item: px(4.),
    };
    for (args, height) in [
        (["tag", "test", oid.as_str()], 65.),
        (["tag", "-d", "test"], 54.),
    ] {
        git(fixture.path(), &args);
        cx.update_window(handle.into(), |_, window, cx| {
            history.scroll_to(anchor);
            panel.update(cx, |panel, cx| panel.refresh(window, cx));
        })?;
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.set_scale_factor(1.);
            window.draw(cx).clear(cx);
            let row = history.bounds_for_item(anchor.item_ix).unwrap();
            assert_eq!(row.size.height, px(height));
            assert_eq!(history.logical_scroll_top(), anchor);
        })?;
    }
    Ok(())
}

fn click(window: &mut Window, cx: &mut App, position: Point<Pixels>) {
    window.simulate_mouse_move(position, cx);
    window.dispatch_event(
        PlatformInput::MouseDown(MouseDownEvent {
            position,
            click_count: 1,
            ..Default::default()
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseUp(MouseUpEvent {
            position,
            click_count: 1,
            ..Default::default()
        }),
        cx,
    );
}

fn overflow_position(window: &Window, mode: ThemeMode, row: Bounds<Pixels>) -> Point<Pixels> {
    let colors = Palette::for_mode(mode);
    let factor = window.scale_factor();
    window
        .painted_quads()
        .into_iter()
        .find(|quad| {
            quad.border_color == colors.border
                && (20. ..35.).contains(&(quad.bounds.size.width.0 / factor))
                && (quad.bounds.size.height.0 / factor - 16.).abs() < 1.
                && quad.bounds.top().0 / factor > row.top().as_f32()
                && quad.bounds.bottom().0 / factor <= row.bottom().as_f32()
                && quad.bounds.right().0 / factor < row.right().as_f32()
        })
        .map(|quad| {
            let center = quad.bounds.center();
            point(px(center.x.0 / factor), px(center.y.0 / factor))
        })
        .expect("a visible +N trigger in the head row")
}

fn render_history_style(
    cx: &mut HeadlessAppContext,
    handle: WindowHandle<Root>,
    panel: &Entity<GitPanel>,
    mode: ThemeMode,
    width: Pixels,
    select_commit: bool,
) -> Result<(Bounds<Pixels>, Bounds<Pixels>, Bounds<Pixels>)> {
    cx.update_window(handle.into(), |_, window, cx| {
        crate::workspace::apply_theme(mode, cx);
        window.set_scale_factor(1.);
        panel.update(cx, |panel, cx| {
            panel.history_width = width;
            panel.selection = Some(if select_commit {
                Selection::Commit(panel.commits[0].oid.to_string(), None)
            } else {
                Selection::Worktree(Mode::All)
            });
            panel.history_scroll.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
            cx.notify();
        });
        window.refresh();
        window.draw(cx).clear(cx);
        let history = panel.read(cx).history_scroll.clone();
        let hover = history
            .bounds_for_item(if select_commit { 0 } else { 2 })
            .unwrap();
        window.simulate_mouse_move(hover.center(), cx);
        window.draw(cx).clear(cx);
        (
            history.bounds_for_item(0).unwrap(),
            history.bounds_for_item(1).unwrap(),
            history.bounds_for_item(3).unwrap(),
        )
    })
}

fn save_history_pane(
    image: &RgbaImage,
    row: Bounds<Pixels>,
    output: &Path,
    name: &str,
) -> Result<()> {
    fs::create_dir_all(output)?;
    let width = row.right().as_f32().round() as u32;
    image::imageops::crop_imm(image, 0, 0, width, image.height())
        .to_image()
        .save(output.join(name))?;
    Ok(())
}

#[::core::prelude::v1::test]
fn commit_history_wraps_titles_and_opens_hidden_refs_without_selecting() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    create_history(fixture.path(), None);
    let state = tempfile::tempdir()?;
    let (mut cx, handle, _, panel) = open_history(fixture.path(), state.path(), None, None)?;
    let output = std::env::var_os("STROLL_HISTORY_SCREENSHOTS").map(PathBuf::from);
    let mut full_height = px(0.);
    for (mode, width, name) in [
        (ThemeMode::Light, px(300.), "fixture-light.png"),
        (ThemeMode::Dark, px(300.), "fixture-dark.png"),
        (ThemeMode::Light, px(230.), "fixture-narrow.png"),
    ] {
        let select_commit = mode.is_dark();
        let (worktree, head, wrapped) =
            render_history_style(&mut cx, handle, &panel, mode, width, select_commit)?;
        let title_height = wrapped.size.height;
        if width == px(300.) {
            full_height = title_height;
        } else {
            assert!(title_height > full_height, "narrow titles gain one line");
            assert!(title_height <= px(68.), "titles are capped at two lines");
        }
        let image = cx.capture_screenshot(handle.into())?;
        let palette = Palette::for_mode(mode);
        let selected = color_bytes(
            palette
                .elevated
                .blend(graph::main_color(mode).opacity(0.09)),
        );
        let (selected_row, hover_y) = if select_commit {
            (head, worktree.top())
        } else {
            (worktree, head.bottom())
        };
        for (top, color) in [
            (selected_row.top(), selected),
            (hover_y, color_bytes(palette.hover)),
        ] {
            let y = top.as_f32() as u32 + 1;
            assert_eq!(image.get_pixel(0, y).0, color, "square left edge");
            // Sample beyond the scrollbar and edge shadow, then check that
            // the underlying row still paints square all the way to the seam.
            assert_eq!(
                image
                    .get_pixel(selected_row.right().as_f32() as u32 - 11, y)
                    .0,
                color,
                "row fill beside the right edge overlays"
            );
            cx.update_window(handle.into(), |_, window, _| {
                let scale = window.scale_factor();
                assert!(
                    window.painted_quads().iter().any(|quad| {
                        quad.bounds.right().0 == selected_row.right().as_f32() * scale
                            && quad.bounds.top().0 <= y as f32 * scale
                            && quad.bounds.bottom().0 > y as f32 * scale
                            && quad.corner_radii.top_right.0 == 0.
                            && quad.background.as_solid().map(color_bytes) == Some(color)
                    }),
                    "square row fill extends to the right seam"
                );
            })?;
        }
        if width == px(230.) {
            assert_track_crosses_seam(&image, 28, wrapped.bottom().as_f32() as u32, 1.);
            let long_row = cx
                .read_entity(&panel, |panel, _| panel.history_scroll.bounds_for_item(8))
                .unwrap();
            assert_eq!(
                long_row.size.height, title_height,
                "a long subject uses the same two-line height"
            );
        }
        if let Some(output) = &output {
            save_history_pane(&image, head, output, name)?;
        }
        if width == px(230.) {
            cx.update_window(handle.into(), |_, window, cx| {
                let target = overflow_position(window, mode, head);
                click(window, cx, target);
                window.draw(cx).clear(cx);
                assert!(matches!(
                    panel.read(cx).selection,
                    Some(Selection::Worktree(_))
                ));
                assert!(
                    window.painted_quads().iter().any(|quad| {
                        quad.bounds.size.width.0 >= 298.
                            && quad.bounds.right().0 > head.right().as_f32()
                            && quad.bounds.top().0 >= head.top().as_f32()
                            && quad.bounds.top().0 < head.bottom().as_f32() + 12.
                    }),
                    "the overflow popup is painted"
                );
            })?;
            if let Some(output) = &output {
                cx.capture_screenshot(handle.into())?
                    .save(output.join("fixture-narrow-overflow.png"))?;
            }
        }
    }
    cx.update_window(handle.into(), |_, window, cx| {
        let history = panel.read(cx).history_scroll.clone();
        history.scroll_to(ListOffset {
            item_ix: 3,
            offset_in_item: px(16.),
        });
        window.draw(cx).clear(cx);
        let before = history.logical_scroll_top();
        panel.update(cx, |panel, cx| {
            panel.history_width = px(300.);
            cx.notify();
        });
        window.refresh();
        window.draw(cx).clear(cx);
        let after = history.logical_scroll_top();
        assert_eq!(after.item_ix, before.item_ix);
        assert!(after.offset_in_item > px(0.));
    })?;
    Ok(())
}

fn preview_geometry(
    paths: &PaintedPaths,
    window: &Window,
    palette: Palette,
    factor: f32,
    row: Bounds<Pixels>,
) -> Option<(Bounds<Pixels>, Point<Pixels>)> {
    let fill: Background = palette.elevated.alpha(0.5).into();
    let card = window.painted_quads().into_iter().find(|quad| {
        quad.background == fill
            && (quad.bounds.size.width.0 / factor - 340.).abs() < 1.
            && quad.bounds.left().0 / factor > row.right().as_f32()
    })?;
    let paths = paths.borrow();
    let arrow_width = window.rem_size().as_f32() * 0.375 + 2. / factor;
    let tip = paths
        .iter()
        .filter(|path| {
            path.color == fill
                && (path.bounds.size.width.0 / factor - arrow_width).abs() < 1.
                && (path.bounds.right().0 - card.bounds.left().0).abs() < 2.
        })
        .flat_map(|path| path.vertices.iter().map(|vertex| vertex.xy_position))
        .min_by(|a, b| a.x.0.total_cmp(&b.x.0))?;
    Some((
        card.bounds.map(|v| px(v.0 / factor)),
        point(px(tip.x.0 / factor), px(tip.y.0 / factor)),
    ))
}

fn show_commit_preview(
    cx: &mut HeadlessAppContext,
    handle: WindowHandle<Root>,
    row: Bounds<Pixels>,
) -> Result<RgbaImage> {
    begin_commit_preview(cx, handle, row)?;
    cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    cx.advance_clock(Duration::from_millis(165));
    std::thread::sleep(Duration::from_millis(165));
    cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    cx.capture_screenshot(handle.into())
}

fn begin_commit_preview(
    cx: &mut HeadlessAppContext,
    handle: WindowHandle<Root>,
    row: Bounds<Pixels>,
) -> Result<()> {
    cx.update_window(handle.into(), |_, window, cx| {
        window.simulate_mouse_move(point(px(600.), px(40.)), cx);
        window.draw(cx).clear(cx);
        window.simulate_mouse_move(row.center(), cx);
        window.draw(cx).clear(cx);
    })?;
    cx.run_until_parked();
    let delay = cx.update(|cx| gpui_component::tooltip::TooltipDefaults::global(cx).show_delay());
    cx.advance_clock(delay);
    cx.run_until_parked();
    Ok(())
}

fn preview_opacity(window: &Window, row: Bounds<Pixels>) -> Option<f32> {
    window.painted_quads().into_iter().find_map(|quad| {
        let fill = quad.background.as_solid()?;
        ((quad.bounds.size.width.0 - 340.).abs() < 1.
            && quad.bounds.left().0 > row.right().as_f32()
            && fill.a > 0.
            && fill.a <= 0.5)
            .then_some(fill.a / 0.5)
    })
}

fn assert_arrow_join(image: &RgbaImage, bounds: Bounds<Pixels>, tip: Point<Pixels>, factor: f32) {
    let x = (bounds.left().as_f32() * factor).round() as u32;
    let y = (tip.y.as_f32() * factor).round() as u32;
    for row in y - 1..=y + 1 {
        // Sample inside the arrow fill. At fractional positions x - 3 can hit
        // the sloping outline instead of the fill.
        let outer = image.get_pixel(x - 2, row).0;
        let inner = image.get_pixel(x + 3, row).0;
        for column in x - 1..=x + 2 {
            let pixel = image.get_pixel(column, row).0;
            for channel in 0..3 {
                let range = (outer[channel].min(inner[channel]).saturating_sub(4))
                    ..=(outer[channel].max(inner[channel]).saturating_add(4));
                assert!(
                    range.contains(&pixel[channel]),
                    "a seam at {factor}x ({column}, {row}): {pixel:?}, {outer:?}, {inner:?}"
                );
            }
        }
    }
}

#[::core::prelude::v1::test]
fn commit_preview_waits_for_pointer_motion_after_scrolling() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    create_history(fixture.path(), None);
    let state = tempfile::tempdir()?;
    let paths = PaintedPaths::default();
    let (mut cx, handle, _, panel) =
        open_history(fixture.path(), state.path(), Some(paths.clone()), None)?;
    let palette = Palette::for_mode(ThemeMode::Light);
    let (first, distance) = cx.update_window(handle.into(), |_, window, cx| {
        crate::workspace::apply_theme(ThemeMode::Light, cx);
        window.set_scale_factor(1.);
        window.draw(cx).clear(cx);
        let history = &panel.read(cx).history_scroll;
        let first = history.bounds_for_item(1).unwrap();
        let next = history.bounds_for_item(2).unwrap();
        (first, next.center().y - first.center().y)
    })?;
    let pointer = first.center();
    show_commit_preview(&mut cx, handle, first)?;
    let arrow_y = |cx: &mut HeadlessAppContext| -> Result<Option<Pixels>> {
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
        cx.capture_screenshot(handle.into())?;
        cx.update_window(handle.into(), |_, window, _| {
            preview_geometry(&paths, window, palette, 1., first).map(|(_, tip)| tip.y)
        })
    };
    let initial = arrow_y(&mut cx)?.expect("original commit preview");
    let scroll = |cx: &mut HeadlessAppContext, dy| -> Result<()> {
        cx.update_window(handle.into(), |_, window, cx| {
            window.dispatch_event(
                ScrollEvent {
                    position: pointer,
                    delta: ScrollDelta::Pixels(point(px(0.), dy)),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.draw(cx).clear(cx);
        })
    };
    scroll(&mut cx, px(-5.))?;
    let shifted = arrow_y(&mut cx)?.expect("same commit retains its preview");
    assert!((initial - shifted - px(5.)).abs() <= px(1.));
    scroll(&mut cx, px(5.) - distance)?;
    cx.run_until_parked();
    cx.advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(arrow_y(&mut cx)?, None);
    cx.update_window(handle.into(), |_, window, cx| {
        window.simulate_mouse_move(pointer, cx);
    })?;
    cx.run_until_parked();
    cx.advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(arrow_y(&mut cx)?, None);
    cx.update_window(handle.into(), |_, window, cx| {
        window.simulate_mouse_move(pointer + point(px(1.), px(0.)), cx);
    })?;
    cx.run_until_parked();
    cx.advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(arrow_y(&mut cx)?.is_some());
    Ok(())
}

#[::core::prelude::v1::test]
fn commit_message_is_previewed_to_the_right_and_visible_in_the_diff() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let body = "Explain why we cache the branch geometry.\n\nKeep the first visible row anchored when its labels change.\nPreserve the original paragraph breaks.";
    create_history(fixture.path(), Some(body));
    fs::write(fixture.path().join("notes.txt"), "A changed line\n")?;
    git(fixture.path(), &["add", "notes.txt"]);
    git(
        fixture.path(),
        &["commit", "-qm", "Keep commit messages readable", "-m", body],
    );
    let state = tempfile::tempdir()?;
    let paths = PaintedPaths::default();
    let pointer_probe = Rc::new(RefCell::new(None));
    let (mut cx, handle, scale, panel) = open_history(
        fixture.path(),
        state.path(),
        Some(paths.clone()),
        Some(pointer_probe.clone()),
    )?;
    cx.update(|cx| cx.set_reduce_motion(false));
    let output = std::env::var_os("STROLL_HISTORY_SCREENSHOTS").map(PathBuf::from);
    let oid = cx.read_entity(&panel, |panel, _| {
        assert_eq!(panel.commits[0].body_preview.as_str(), body);
        panel.commits[0].oid.clone()
    });
    for (mode, theme) in [(ThemeMode::Light, "light"), (ThemeMode::Dark, "dark")] {
        let palette = Palette::for_mode(mode);
        for factor in [1., 1.25, 1.5, 2.] {
            scale.set(factor);
            let (head, edge) = cx.update_window(handle.into(), |_, window, cx| {
                crate::workspace::apply_theme(mode, cx);
                window.set_scale_factor(factor);
                window.draw(cx).clear(cx);
                let history = &panel.read(cx).history_scroll;
                let viewport = history.viewport_bounds();
                let edge = history
                    .visible_range()
                    .rev()
                    .filter_map(|ix| history.bounds_for_item(ix))
                    .find(|row| {
                        row.center().y < viewport.bottom()
                            && row.top() > viewport.bottom() - px(85.)
                    })
                    .expect("a visible row near the bottom");
                (history.bounds_for_item(1).unwrap(), edge)
            })?;
            for (row, suffix) in [(head, ""), (edge, "-edge")] {
                let shot = show_commit_preview(&mut cx, handle, row)?;
                if let Some(output) = &output {
                    fs::create_dir_all(output)?;
                    shot.save(output.join(format!(
                        "commit-hover-{theme}-{}{suffix}.png",
                        (factor * 100.) as u32
                    )))?;
                }
                let (bounds, tip) = cx
                    .update_window(handle.into(), |_, window, _| {
                        preview_geometry(&paths, window, palette, factor, row)
                    })?
                    .unwrap_or_else(|| {
                        panic!(
                            "a preview to the right of the history at {theme} {factor}x {suffix}"
                        )
                    });
                assert_arrow_join(&shot, bounds, tip, factor);
                assert!(bounds.left() > row.right());
                assert!(bounds.right() <= WINDOW_SIZE.width - px(7.));
                assert!(bounds.bottom() <= WINDOW_SIZE.height - px(7.));
                assert!(
                    tip.y < row.center().y,
                    "the tip aims above the row midpoint: {tip:?} {row:?}"
                );
                assert!(
                    tip.y >= row.top() - px(1.),
                    "the tip stays close to the title"
                );
                if suffix.is_empty() {
                    assert!(
                        bounds.size.height >= px(100.),
                        "the preview includes body paragraphs"
                    );
                    assert!(
                        (tip.y - row.top() - px(14.)).abs() <= px(1.5),
                        "the tip is centered on the first title line"
                    );
                    let inset = tip.y - bounds.top();
                    assert!(
                        (px(17.)..=px(19.)).contains(&inset),
                        "the arrow sits near the top: {inset:?}"
                    );
                } else {
                    assert!(
                        bounds.bottom() >= WINDOW_SIZE.height - px(9.),
                        "the lower card is clamped to the viewport"
                    );
                    assert!(
                        tip.y < bounds.bottom() - px(12.),
                        "the tip avoids the rounded corner"
                    );
                }
            }
        }
    }

    scale.set(1.);
    let head = cx.update_window(handle.into(), |_, window, cx| {
        crate::workspace::apply_theme(ThemeMode::Light, cx);
        window.set_scale_factor(1.);
        window.draw(cx).clear(cx);
        panel.read(cx).history_scroll.bounds_for_item(1).unwrap()
    })?;
    cx.update_window(handle.into(), |_, window, cx| {
        window.simulate_mouse_move(point(px(600.), px(40.)), cx);
        window.draw(cx).clear(cx);
    })?;
    std::thread::sleep(Duration::from_millis(100));
    cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    begin_commit_preview(&mut cx, handle, head)?;
    std::thread::sleep(Duration::from_millis(20));
    let entering = cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        preview_opacity(window, head).expect("a partially visible entering preview")
    })?;
    assert!(
        (0.01..0.99).contains(&entering),
        "entrance opacity: {entering}"
    );
    let entrance = cx.capture_screenshot(handle.into())?;

    cx.advance_clock(Duration::from_millis(165));
    std::thread::sleep(Duration::from_millis(165));
    cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    let settled = cx.capture_screenshot(handle.into())?;
    let palette = Palette::for_mode(ThemeMode::Light);
    let (bounds, _) = cx
        .update_window(handle.into(), |_, window, _| {
            assert_eq!(preview_opacity(window, head), Some(1.));
            preview_geometry(&paths, window, palette, 1., head)
        })?
        .unwrap();
    let pointer = bounds.center();
    cx.update_window(handle.into(), |_, window, cx| {
        let hitbox = pointer_probe.borrow();
        assert!(hitbox.as_ref().unwrap().is_hovered_at(pointer, window));
        window.simulate_mouse_move(pointer, cx);
    })?;
    std::thread::sleep(Duration::from_millis(35));
    let leaving = cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(pointer_probe.borrow().as_ref().unwrap().is_hovered(window));
        preview_opacity(window, head).expect("a partially visible exiting preview")
    })?;
    assert!((0.01..0.99).contains(&leaving), "exit opacity: {leaving}");
    let exit = cx.capture_screenshot(handle.into())?;

    std::thread::sleep(Duration::from_millis(100));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(
            preview_opacity(window, head).is_none(),
            "the card itself does not extend hover"
        );
    })?;
    let gone = cx.capture_screenshot(handle.into())?;
    assert_ne!(entrance, settled, "the GPU draws a distinct entrance frame");
    assert_ne!(exit, settled, "the GPU draws a distinct exit frame");
    assert_ne!(exit, gone, "the GPU keeps drawing during exit");
    if let Some(output) = &output {
        for (name, image) in [
            ("enter", entrance),
            ("settled", settled),
            ("exit", exit),
            ("gone", gone),
        ] {
            image.save(output.join(format!("commit-hover-motion-{name}.png")))?;
        }
    }
    let (worktree, next) = cx.read_entity(&panel, |panel, _| {
        (
            panel.history_scroll.bounds_for_item(0).unwrap(),
            panel.history_scroll.bounds_for_item(2).unwrap(),
        )
    });
    begin_commit_preview(&mut cx, handle, worktree)?;
    cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    cx.advance_clock(Duration::from_millis(25));
    std::thread::sleep(Duration::from_millis(130));
    let preview_top = |cx: &mut HeadlessAppContext, at| -> Result<Pixels> {
        cx.update_window(handle.into(), |_, window, cx| {
            if let Some(at) = at {
                window.simulate_mouse_move(at, cx);
            }
            window.draw(cx).clear(cx);
        })?;
        cx.capture_screenshot(handle.into())?;
        cx.update_window(handle.into(), |_, window, _| {
            preview_geometry(&paths, window, palette, 1., worktree)
                .expect("the preview stays visible while switching")
                .0
                .top()
        })
    };
    let entering_top = preview_top(&mut cx, None)?;
    assert_eq!(
        preview_top(&mut cx, Some(head.center()))?,
        entering_top,
        "switching during entrance starts at the painted position"
    );
    cx.advance_clock(Duration::from_millis(100));
    let halfway = preview_top(&mut cx, None)?;
    assert!(
        halfway > entering_top,
        "the tooltip moves toward the next row"
    );
    assert_eq!(preview_top(&mut cx, Some(worktree.center()))?, halfway);
    cx.advance_clock(Duration::from_millis(200));
    let settled_top = preview_top(&mut cx, None)?;
    assert!(
        entering_top > settled_top && entering_top - settled_top < px(4.),
        "the handoff occurred partway through the entrance: {entering_top:?} → {settled_top:?}"
    );

    cx.update(|cx| cx.set_reduce_motion(true));
    for (row, name) in [
        (worktree, "worktree"),
        (head, "head"),
        (next, "next"),
        (worktree, "back-to-worktree"),
    ] {
        cx.update_window(handle.into(), |_, window, cx| {
            // Hover the graph side, outside the old worktree label trigger.
            window.simulate_mouse_move(point(row.left() + px(5.), row.center().y), cx);
            window.draw(cx).clear(cx);
        })?;
        let image = cx.capture_screenshot(handle.into())?;
        let (bounds, tip) = cx
            .update_window(handle.into(), |_, window, _| {
                preview_geometry(&paths, window, palette, 1., row)
            })?
            .unwrap_or_else(|| panic!("the {name} preview appears without another show delay"));
        assert!(
            tip.y >= row.top() && tip.y < row.bottom(),
            "{name}: {tip:?} {row:?}"
        );
        if row == worktree {
            assert!(
                bounds.size.height < px(75.),
                "the worktree preview only shows its summary"
            );
        }
        if let Some(output) = &output {
            image.save(output.join(format!("history-preview-switch-{name}.png")))?;
        }
    }
    cx.update(|cx| cx.set_reduce_motion(false));
    cx.update_window(handle.into(), |_, window, cx| {
        window.simulate_mouse_move(point(px(600.), px(65.)), cx);
        window.draw(cx).clear(cx);
    })?;
    cx.update_window(handle.into(), |_, window, cx| {
        window.simulate_mouse_move(head.center(), cx);
        window.draw(cx).clear(cx);
    })?;
    cx.run_until_parked();
    let delay = cx.update(|cx| gpui_component::tooltip::TooltipDefaults::global(cx).show_delay());
    cx.advance_clock(delay - Duration::from_millis(1));
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(100));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(
            preview_opacity(window, head).is_none(),
            "returning during exit still waits"
        );
    })?;
    cx.advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(130));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert_eq!(preview_opacity(window, head), Some(1.));
    })?;
    cx.update_window(handle.into(), |_, window, cx| {
        let second = panel.read(cx).history_scroll.bounds_for_item(2).unwrap();
        let neighbor = panel.read(cx).commits[1].oid.clone();
        click(window, cx, second.center());
        assert!(matches!(&panel.read(cx).selection, Some(Selection::Commit(id, _)) if id == neighbor.as_str()));
        click(window, cx, head.center());
        assert!(matches!(&panel.read(cx).selection, Some(Selection::Commit(id, _)) if id == oid.as_str()));
    })?;
    for _ in 0..4 {
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    }
    cx.update_window(handle.into(), |_, window, cx| {
        let panel = panel.read(cx);
        assert!(!panel.show_details);
        let review = panel.review.as_ref().unwrap();
        assert!(!review.detail.files.is_empty());
        let body = panel
            .editor
            .focused_column(window, cx)
            .read(cx)
            .projected_line_bounds(0)
            .expect("the diff summary displays the body without opening details");
        assert!(body.size.height >= px(100.));
        let first_line = panel
            .review_source_bounds(
                &DiffLinePosition::new("notes.txt", DiffSide::Modified, 1),
                cx,
            )
            .expect("the first changed line");
        let gap = first_line.top() - body.bottom();
        assert!(
            (px(28.)..=px(75.)).contains(&gap),
            "the first changed line follows the summary and file headers: {gap:?}"
        );
    })?;
    if let Some(output) = output {
        cx.capture_screenshot(handle.into())?
            .save(output.join("commit-diff-body.png"))?;
    }
    Ok(())
}

#[::core::prelude::v1::test]
fn staging_keeps_the_diff_viewport_and_checkbox_colors() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = fixture.path();
    let path = "src/a-review.txt";
    let other = "src/b-other.txt";
    fs::create_dir(root.join("src"))?;
    fs::write(root.join(path), "original\n")?;
    fs::write(root.join(other), "before\n")?;
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Reviewer"]);
    git(root, &["config", "user.email", "reviewer@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "Initial changes"]);
    let text = (1..=150)
        .map(|line| format!("Updated review content, line {line}\n"))
        .collect::<String>();
    fs::write(root.join(path), &text)?;
    fs::write(root.join(other), "other retained change\n")?;
    let (mut cx, handle, _, panel) = open_history(root, state.path(), None, None)?;
    let window = handle.into();
    let image_dir = std::env::var_os("STROLL_GIT_SCREENSHOTS").map(PathBuf::from);
    if let Some(dir) = &image_dir {
        fs::create_dir_all(dir)?;
        cx.update_window(window, |_, window, cx| {
            window.set_scale_factor(1.);
            window.draw(cx).clear(cx);
        })?;
        cx.run_until_parked();
        cx.capture_screenshot(window)?
            .save(dir.join("worktree-summary.png"))?;
    }
    let line = DiffLinePosition::new(path, DiffSide::Modified, 64);
    cx.update_window(window, |_, window, cx| {
        window.set_scale_factor(1.);
        panel.update(cx, |panel, cx| {
            panel.request_patch(1, window, cx);
            panel
                .diff
                .update(cx, |diff, cx| diff.scroll_to_line(line.clone(), cx));
        });
    })?;
    for _ in 0..4 {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))?;
    }
    let bounds = |cx: &mut HeadlessAppContext| {
        cx.read_entity(&panel, |panel, cx| {
            let tree = panel.tree_scroll.0.borrow().base_handle.bounds();
            (panel.review_source_bounds(&line, cx).unwrap(), tree)
        })
    };
    let before_bounds = bounds(&mut cx);
    let before = cx.capture_screenshot(window)?;
    if let Some(dir) = &image_dir {
        before.save(dir.join("stage-before.png"))?;
    }

    for staged in [true, false] {
        cx.update_window(window, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.stage_files(vec![path.into()], staged, window, cx)
            });
        })?;
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))?;
        let pending = cx.capture_screenshot(window)?;
        let pending_bounds = bounds(&mut cx);
        assert_eq!(
            pending_bounds, before_bounds,
            "staging must leave the viewport in place"
        );
        if staged {
            let tree = before_bounds.1;
            for y in (tree.top().as_f32() as u32 + 6)..(tree.top().as_f32() as u32 + 75) {
                for x in (tree.right().as_f32() as u32 - 32)..(tree.right().as_f32() as u32 - 16) {
                    assert_eq!(
                        pending.get_pixel(x, y),
                        before.get_pixel(x, y),
                        "checkbox pixel ({x}, {y})"
                    );
                }
            }
            if let Some(dir) = &image_dir {
                pending.save(dir.join("stage-pending.png"))?;
            }
        }
        for _ in 0..4 {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))?;
            let after_bounds = bounds(&mut cx);
            assert!(
                (after_bounds.0.top() - before_bounds.0.top()).abs() <= px(1.),
                "source line moved from {:?} to {:?}",
                before_bounds.0,
                after_bounds.0
            );
            assert_eq!(after_bounds.1, before_bounds.1);
        }
        cx.read_entity(&panel, |panel, _| {
            let review = panel.review.as_ref().unwrap();
            assert_eq!(review.detail.status.as_ref().unwrap()[path].staged, staged);
            assert!(matches!(review.patches[1], PatchState::Ready));
        });
        if staged && let Some(dir) = &image_dir {
            cx.capture_screenshot(window)?
                .save(dir.join("stage-after.png"))?;
        }
    }
    assert_eq!(fs::read_to_string(root.join(path))?, text);
    Ok(())
}

#[::core::prelude::v1::test]
#[ignore = "requires an SSH worktree and a disposable index"]
fn ssh_stage_checkbox_round_trip() -> Result<()> {
    let root = PathBuf::from(std::env::var("STROLL_GIT_REMOTE_FIXTURE")?);
    let path = std::env::var("STROLL_GIT_REMOTE_FILE")?;
    let state = tempfile::tempdir()?;
    let (mut cx, handle, _, panel) = open_history(&root, state.path(), None, None)?;
    let window = handle.into();
    cx.update_window(window, |_, window, cx| {
        window.set_scale_factor(1.);
        panel.update(cx, |panel, cx| {
            panel.filter = path.clone();
            cx.notify();
        });
    })?;
    cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))?;
    let click = cx.read_entity(&panel, |panel, _| {
        let tree = panel.tree_scroll.0.borrow().base_handle.bounds();
        let review = panel.review.as_ref().unwrap();
        let ix = review.by_path[&path];
        let row = review
            .tree_rows
            .borrow()
            .as_ref()
            .unwrap()
            .file_row(ix)
            .unwrap();
        point(
            tree.right() - px(23.),
            tree.top() + px(28. * row as f32 + 14.),
        )
    });
    for staged in [true, false] {
        let start = Instant::now();
        cx.update_window(window, |_, window, cx| {
            window.simulate_mouse_move(click, cx);
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: click,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: click,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
        })?;
        for _ in 0..4 {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))?;
        }
        cx.read_entity(&panel, |panel, _| {
            assert!(panel.stage_error.is_none(), "{:?}", panel.stage_error);
            let review = panel.review.as_ref().unwrap();
            let status = review.detail.status.as_ref().unwrap();
            assert_eq!(status[&path].staged, staged);
            assert_eq!(
                status.values().filter(|state| state.staged).count(),
                usize::from(staged)
            );
        });
        eprintln!(
            "SSH checkbox staged={staged}: {:.1} ms through settled frames",
            start.elapsed().as_secs_f64() * 1000.
        );
        if staged && let Some(dir) = std::env::var_os("STROLL_GIT_SCREENSHOTS") {
            let dir = PathBuf::from(dir);
            fs::create_dir_all(&dir)?;
            cx.capture_screenshot(window)?
                .save(dir.join("ssh-staged.png"))?;
        }
    }
    Ok(())
}

#[::core::prelude::v1::test]
fn partial_stage_keeps_surviving_line_in_place_offscreen() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let state = tempfile::tempdir()?;
    let root = fixture.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Reviewer"]);
    git(root, &["config", "user.email", "reviewer@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    let path = "partially-staged.txt";
    let text = |dirty| {
        (1..=130)
            .map(|line| {
                let changed = dirty
                    && ((32..36).contains(&line)
                        || (58..63).contains(&line)
                        || (98..104).contains(&line));
                format!(
                    "{} source line {line}\n",
                    if changed { "edited" } else { "original" }
                )
            })
            .collect::<String>()
    };
    fs::write(root.join(path), text(false))?;
    git(root, &["add", path]);
    git(root, &["commit", "-qm", "Initial changes"]);
    fs::write(root.join(path), text(true))?;
    let (mut cx, handle, _, panel) = open_history(root, state.path(), None, None)?;
    let window = handle.into();
    let draw = |cx: &mut HeadlessAppContext| -> Result<()> {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
    };
    cx.update_window(window, |_, window, cx| {
        window.set_scale_factor(1.);
        panel.update(cx, |panel, cx| {
            panel.show_history = false;
            panel.split = true;
            panel.wrap = false;
            cx.notify();
            panel.request_patch(0, window, cx);
        });
    })?;
    for _ in 0..4 {
        draw(&mut cx)?;
    }
    let anchor = DiffLinePosition::new(path, DiffSide::Modified, 34);
    cx.update_window(window, |_, _, cx| {
        panel
            .read(cx)
            .diff
            .clone()
            .update(cx, |diff, cx| diff.scroll_to_line(anchor.clone(), cx));
    })?;
    for _ in 0..4 {
        draw(&mut cx)?;
    }
    let y = |cx: &mut HeadlessAppContext, position: &DiffLinePosition| {
        cx.read_entity(&panel, |panel, cx| {
            panel.review_source_bounds(position, cx).unwrap().top()
        })
    };
    let operated = DiffLinePosition::new(path, DiffSide::Modified, 58);
    let before = (y(&mut cx, &anchor), y(&mut cx, &operated));
    let output = std::env::var_os("STROLL_GIT_SCREENSHOTS").map(PathBuf::from);
    if let Some(output) = &output {
        fs::create_dir_all(output)?;
        cx.capture_screenshot(window)?
            .save(output.join("partial-stage-before.png"))?;
    }
    let changes = cx.read_entity(&panel, |panel, _| {
        panel.backend.file_changes(
            path,
            git::FileDiffOptions {
                mode: Mode::All,
                context: 0,
                whitespace: false,
            },
        )
    })?;
    let block = changes.layers[0].blocks[1].target.clone();
    assert!(block.is_some());
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_worktree_file(
                git::FileEdit {
                    path: path.into(),
                    version: changes.version,
                    action: git::FileAction::Stage,
                    block,
                },
                window,
                cx,
            )
        });
    })?;
    for _ in 0..5 {
        draw(&mut cx)?;
    }
    assert_eq!((y(&mut cx, &anchor), y(&mut cx, &operated)), before);
    if let Some(output) = output {
        cx.capture_screenshot(window)?
            .save(output.join("partial-stage-after.png"))?;
    }
    assert_eq!(fs::read_to_string(root.join(path))?, text(true));
    Ok(())
}

#[::core::prelude::v1::test]
fn consecutive_commit_selection_shows_net_diff_and_tree() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repo");
    fs::create_dir(&root)?;
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let commit = |message| -> Result<String> {
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", message]);
        Ok(git::head(&root)?.unwrap())
    };
    fs::write(root.join("base.txt"), "base\n")?;
    let base = commit("Base")?;
    fs::write(root.join("first.txt"), "first\n")?;
    fs::write(root.join("cancelled.txt"), "temporary\n")?;
    let first = commit("First")?;
    fs::remove_file(root.join("cancelled.txt"))?;
    fs::write(root.join("second.txt"), "second\n")?;
    let second = commit("Second")?;
    fs::write(root.join("transient.txt"), "temporary\n")?;
    commit("Add transient file")?;
    fs::remove_file(root.join("transient.txt"))?;
    let latest = commit("Remove transient file")?;

    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let (workspace, visual) = app.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
    });
    let draw = |visual: &mut VisualTestContext| {
        for _ in 0..6 {
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
        }
    };
    visual.simulate_resize(size(px(1280.), px(760.)));
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(Location { root, file: None }, window, cx)
        });
    });
    draw(visual);
    let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
    visual.update(|_, cx| panel.update(cx, |panel, cx| panel.set_tree_visible(true, cx)));
    draw(visual);
    let row = |visual: &mut VisualTestContext, index| {
        visual.update(|_, cx| {
            panel
                .read(cx)
                .history_scroll
                .bounds_for_item(index)
                .unwrap()
                .center()
        })
    };
    let latest_row = row(visual, 1);
    let previous_row = row(visual, 2);
    let second_row = row(visual, 3);
    let first_row = row(visual, 4);
    let range = Modifiers {
        shift: true,
        ..Default::default()
    };
    let additive = Modifiers {
        control: true,
        ..Default::default()
    };

    visual.simulate_click(second_row, Modifiers::default());
    visual.simulate_click(first_row, range);
    draw(visual);
    assert!(visual.debug_bounds("git-commit-range-comparison").is_some());
    assert!(visual.debug_bounds("git-tree-row:file:first.txt").is_some());
    assert!(
        visual
            .debug_bounds("git-tree-row:file:second.txt")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("git-tree-row:file:cancelled.txt")
            .is_none()
    );
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        let review = panel.review.as_ref().unwrap();
        assert!(matches!(&review.detail.comparison,
            Comparison::Versions { base: Some(from), target: Some(to) } if from == &base && to == &second));
        assert_eq!(panel.diff.read(cx).files().iter().map(|file| file.path().as_str()).collect::<Vec<_>>(), ["first.txt", "second.txt"]);
        assert!(review.documents.iter().any(|file| file.hunk_locations().next().is_some()));
        assert_eq!(panel.session(cx).unwrap().selection, Some(Selection::Commit(first.clone(), None)));
    });

    visual.simulate_click(second_row, Modifiers::default());
    draw(visual);
    assert!(visual.debug_bounds("git-commit-range-comparison").is_none());
    assert!(
        visual
            .debug_bounds("git-tree-row:file:cancelled.txt")
            .is_some()
    );

    visual.simulate_click(latest_row, Modifiers::default());
    visual.simulate_click(previous_row, range);
    draw(visual);
    assert!(visual.debug_bounds("git-commit-range-comparison").is_some());
    assert!(
        visual
            .debug_bounds("git-tree-row:file:transient.txt")
            .is_none()
    );
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(panel.review.as_ref().unwrap().detail.files.is_empty());
        assert!(panel.diff.read(cx).files().is_empty());
    });

    visual.simulate_click(second_row, Modifiers::default());
    visual.simulate_click(latest_row, additive);
    draw(visual);
    assert!(visual.debug_bounds("git-history-selected-count").is_some());
    assert!(visual.debug_bounds("git-commit-range-comparison").is_none());
    assert!(
        visual
            .debug_bounds("git-tree-row:file:transient.txt")
            .is_some()
    );
    visual.update(|_, cx| {
        assert_eq!(
            panel.read(cx).selection,
            Some(Selection::Commit(latest.clone(), None))
        )
    });
    Ok(())
}

#[::core::prelude::v1::test]
fn commit_markers_share_the_editor_gutter_and_keep_local_priority() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repo");
    fs::create_dir(&root)?;
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let path = PathBuf::from("review.txt");
    let write = |first, staged, unstaged, last| {
        fs::write(
            root.join(&path),
            format!(
                "start\n{first}\n{staged}\nseparator\n{unstaged}\nsecond separator\n{last}\nend\n{}",
                "tail\n".repeat(80)
            ),
        )
    };
    let commit = |subject| {
        git(&root, &["add", "--", "review.txt"]);
        git(&root, &["commit", "-qm", subject]);
    };
    write("old first", "old staged", "old unstaged", "old last")?;
    commit("Base");
    write("new first", "new staged", "old unstaged", "old last")?;
    commit("First selected commit");
    write("new first", "new staged", "new unstaged", "new last")?;
    commit("Second selected commit");
    write("new first", "old staged", "old unstaged", "new last")?;
    commit("Update two lines after the selected commits");
    write("new first", "new staged", "old unstaged", "new last")?;
    git(&root, &["add", "--", "review.txt"]);
    write("new first", "new staged", "new unstaged", "new last")?;

    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    let draw = |visual: &mut VisualTestContext| {
        for _ in 0..6 {
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
        }
    };
    visual.simulate_resize(size(px(1280.), px(760.)));
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root: root.clone(),
                    file: Some(path.clone()),
                },
                window,
                cx,
            )
        });
    });
    draw(visual);
    let original = visual.update(|_, cx| {
        workspace.read(cx).files[&path]
            .editor
            .read(cx)
            .input_bounds()
    });
    let switch = |visual: &mut VisualTestContext, view| {
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.switch_view(view, window, cx))
        });
        draw(visual);
    };
    switch(visual, WorkspaceView::Git);
    let row = |visual: &mut VisualTestContext, index| {
        visual.update(|_, cx| {
            workspace
                .read(cx)
                .git
                .as_ref()
                .unwrap()
                .read(cx)
                .history_scroll
                .bounds_for_item(index)
                .unwrap()
                .center()
        })
    };
    let newer = row(visual, 2);
    let older = row(visual, 3);
    let base = row(visual, 4);
    let right_click = |visual: &mut VisualTestContext, point| {
        visual.simulate_mouse_down(point, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(point, MouseButton::Right, Modifiers::default());
        draw(visual);
        assert!(visual.debug_bounds("popup-menu-surface").is_some());
    };

    let history_color = |cx: &App| {
        let color = if cx.theme().mode == ThemeMode::Dark {
            rgb(0xd77bfa)
        } else {
            rgb(0x963bc7)
        };
        Background::from(Hsla::from(color))
    };
    right_click(visual, newer);
    visual.simulate_keystrokes("up enter");
    draw(visual);
    assert!(visual.debug_bounds("notification-message").is_some());
    visual.update(|window, cx| {
        use gpui_component::WindowExt as _;
        assert_eq!(window.notifications(cx).len(), 1);
        window.clear_notifications(cx);
    });
    switch(visual, WorkspaceView::Editor);
    visual.update(|window, cx| {
        let editor = workspace.read(cx).files[&path].editor.read(cx);
        let gutter_limit = editor.input_bounds().left() + px(16.);
        let x = |value: ScaledPixels| px(value.0 / window.scale_factor());
        let quads = window.painted_quads();
        let historical = quads
            .iter()
            .filter(|quad| quad.background == history_color(cx));
        assert_eq!(
            historical
                .clone()
                .filter(|quad| x(quad.bounds.left()) < gutter_limit)
                .count(),
            1
        );
        let ruler_marks = historical
            .filter(|quad| x(quad.bounds.left()) > original.right() - px(16.))
            .collect::<Vec<_>>();
        assert!(
            !ruler_marks.is_empty(),
            "history is also painted on the right track"
        );
        let mixed = Background::from(Palette::for_mode(cx.theme().mode).accent);
        let local_marks = quads.iter().filter(|quad| {
            quad.background == mixed
                && x(quad.bounds.left()) > original.right() - px(16.)
                && x(quad.bounds.size.width) == px(3.)
        });
        let left_lane = local_marks
            .clone()
            .next()
            .expect("the local Git overview lane");
        assert!(
            ruler_marks
                .iter()
                .all(|quad| quad.bounds.left() == left_lane.bounds.left()
                    && quad.bounds.size.width == left_lane.bounds.size.width),
            "history shares the local Git overview lane"
        );
        assert!(
            local_marks
                .into_iter()
                .any(|local| ruler_marks.iter().any(|history| {
                    !history.bounds.intersect(&local.bounds).is_empty()
                        && history.order < local.order
                })),
            "local marks paint on top where the overview markers overlap"
        );
    });
    switch(visual, WorkspaceView::Git);

    visual.simulate_click(newer, Modifiers::default());
    visual.simulate_click(
        base,
        Modifiers {
            control: true,
            ..Default::default()
        },
    );
    draw(visual);
    visual.update(|_, cx| {
        let panel = workspace.read(cx).git.as_ref().unwrap().read(cx);
        assert!(matches!(
            panel.mark_commits_item(&panel.commits[1].oid),
            PopupMenuItem::Item { disabled: true, .. }
        ));
    });

    visual.simulate_click(newer, Modifiers::default());
    visual.simulate_click(
        older,
        Modifiers {
            shift: true,
            ..Default::default()
        },
    );
    draw(visual);
    right_click(visual, newer);
    visual.simulate_keystrokes("up enter");
    draw(visual);
    assert!(visual.update(|_, cx| workspace.read(cx).commit_markers.active()));
    switch(visual, WorkspaceView::Editor);

    let markers = visual.update(|window, cx| {
        let editor = workspace.read(cx).files[&path].editor.read(cx);
        assert_eq!(editor.input_bounds(), original);
        let quads = window.painted_quads();
        let scale = window.scale_factor();
        let history = history_color(cx);
        let purple = quads
            .iter()
            .filter(|quad| {
                quad.background == history
                    && px(quad.bounds.left().0 / scale) < original.left() + px(16.)
            })
            .collect::<Vec<_>>();
        let x = px(purple
            .first()
            .expect("selected commits paint historical marks")
            .bounds
            .left()
            .0
            / scale);
        let at_line = |line| {
            point(
                x + px(14.),
                editor.projected_line_bounds(line).unwrap().center().y,
            )
        };
        for line in [1, 6] {
            assert!(
                purple
                    .iter()
                    .any(|quad| px(quad.bounds.top().0 / scale) <= at_line(line).y
                        && px(quad.bounds.bottom().0 / scale) >= at_line(line).y)
            );
        }
        for line in [2, 4] {
            let y = at_line(line).y;
            assert!(
                !purple
                    .iter()
                    .any(|quad| px(quad.bounds.top().0 / scale) <= y
                        && px(quad.bounds.bottom().0 / scale) >= y)
            );
        }
        (
            at_line(1),
            at_line(2),
            at_line(4),
            point(original.left() + px(80.), at_line(0).y),
        )
    });

    visual.simulate_click(markers.0, Modifiers::default());
    draw(visual);
    visual.update(|window, cx| {
        let hunk = super::super::super::code_theme::HunkPalette::for_mode(cx.theme().mode);
        let quads = window.painted_quads();
        for color in [hunk.removed_row, hunk.added_row] {
            assert!(
                quads
                    .iter()
                    .any(|quad| quad.background == Background::from(color))
            );
        }
    });
    assert!(visual.debug_bounds("git-hunk-action:0").is_none());
    visual.simulate_mouse_move(markers.3, None, Modifiers::default());
    draw(visual);
    assert!(visual.debug_bounds("editor-git-hunk-status").is_some());
    let previous_x = visual
        .debug_bounds("editor-git-previous-hunk:history-0")
        .unwrap()
        .center()
        .x;
    let next_x = visual
        .debug_bounds("editor-git-next-hunk:history-0")
        .unwrap()
        .center()
        .x;
    let navigation = |visual: &mut VisualTestContext, selector: &'static str| {
        let button = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing hunk navigation control {selector}"));
        let toolbar = visual.debug_bounds("editor-git-hunk-controls").unwrap();
        let expected_x = if selector.starts_with("editor-git-next") {
            assert_eq!(button.right(), toolbar.right());
            next_x
        } else {
            previous_x
        };
        assert_eq!(
            button.center().x,
            expected_x,
            "{selector} keeps the same click column"
        );
        visual.simulate_click(button.center(), Modifiers::default());
        draw(visual);
        visual.update(|_, cx| {
            assert_eq!(workspace.read(cx).tabs.active(), Some(path.as_path()));
        });
    };
    navigation(visual, "editor-git-previous-hunk:history-0");
    navigation(visual, "editor-git-next-hunk:history-0");
    assert!(visual.debug_bounds("git-hunk-action:1").is_some());
    navigation(visual, "editor-git-next-hunk:0");
    assert!(visual.debug_bounds("git-hunk-action:0").is_some());
    navigation(visual, "editor-git-next-hunk:1");
    navigation(visual, "editor-git-next-hunk:history-1");
    navigation(visual, "editor-git-next-hunk:history-2");
    navigation(visual, "editor-git-previous-hunk:history-2");
    navigation(visual, "editor-git-previous-hunk:history-1");
    assert!(visual.debug_bounds("git-hunk-action:0").is_some());
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(visual.debug_bounds("editor-git-hunk-controls").is_none());
    // Navigation keeps the clicked label at its screen position. Return to
    // the viewport where the gutter positions below were measured.
    visual.update(|_, cx| {
        let editor = workspace.read(cx).files[&path].editor.clone();
        editor.update(cx, |editor, cx| {
            editor.set_scroll_offset(point(px(0.), px(0.)), cx);
        });
    });
    draw(visual);
    for (mark, action) in [
        (markers.1, "git-hunk-action:1"),
        (markers.2, "git-hunk-action:0"),
    ] {
        visual.simulate_mouse_move(mark, None, Modifiers::default());
        visual.simulate_click(mark, Modifiers::default());
        draw(visual);
        assert!(
            visual.debug_bounds(action).is_some(),
            "local action {action} has priority"
        );
        visual.simulate_keystrokes("escape");
        draw(visual);
    }

    right_click(visual, markers.3);
    visual.simulate_keystrokes("up enter");
    draw(visual);
    visual.update(|window, cx| {
        assert!(!workspace.read(cx).commit_markers.active());
        assert!(
            window
                .painted_quads()
                .iter()
                .all(|quad| quad.background != history_color(cx))
        );
    });
    Ok(())
}

#[::core::prelude::v1::test]
fn foreign_commit_menu_cherry_picks_single_or_multiple_commits_and_offers_undo() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repo");
    fs::create_dir(&root)?;
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    fs::write(root.join("base"), "base\n")?;
    git(&root, &["add", "base"]);
    git(&root, &["commit", "-qm", "Base"]);
    git(&root, &["switch", "-qc", "source"]);
    fs::write(root.join("picked"), "picked\n")?;
    git(&root, &["add", "picked"]);
    git(&root, &["commit", "-qm", "Pick this change"]);
    fs::write(root.join("picked"), "picked again\n")?;
    fs::write(root.join("base"), "source version\n")?;
    git(&root, &["add", "picked", "base"]);
    git(&root, &["commit", "-qm", "Pick this next"]);
    let source = git::head(&root)?.unwrap();
    git(&root, &["switch", "-q", "main"]);
    fs::write(root.join("current"), "current\n")?;
    git(&root, &["add", "current"]);
    git(&root, &["commit", "-qm", "Current branch change"]);
    let before = git::head(&root)?.unwrap();

    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let platform = gpui_platform::current_platform(true);
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    let mut workspace = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Git, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(1280.), px(760.)));
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root: root.clone(),
                    file: None,
                },
                window,
                cx,
            );
        });
    });
    let draw = |visual: &mut VisualTestContext| {
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.simulate_next_frame(cx);
            });
        }
    };
    draw(visual);
    let panel = visual.update(|window, cx| {
        let panel = workspace.read(cx).git.clone().unwrap();
        panel.update(cx, |panel, cx| {
            panel.select_branch("refs/heads/source".into(), window, cx);
        });
        panel
    });
    draw(visual);
    let row = |visual: &mut VisualTestContext, index| {
        visual.update(|_, cx| {
            panel
                .read(cx)
                .history_scroll
                .bounds_for_item(index)
                .unwrap()
                .center()
        })
    };
    let open_menu = |visual: &mut VisualTestContext, index| {
        let cell = row(visual, index);
        visual.simulate_mouse_down(cell, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(cell, MouseButton::Right, Modifiers::default());
        draw(visual);
        visual
            .debug_bounds("popup-menu-surface")
            .unwrap()
            .size
            .height
    };
    let shared_menu = open_menu(visual, 3);
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(open_menu(visual, 2) > shared_menu);
    assert!(visual.debug_bounds("git-history-rewrite-undo").is_none());

    // The cherry-pick item is immediately above Mark Commit.
    visual.simulate_keystrokes("up up enter");
    draw(visual);
    let picked = git::commit(&root, &git::head(&root)?.unwrap())?;
    assert_eq!(picked.subject, "Pick this change");
    assert_eq!(picked.parents, [before.clone()]);
    assert_eq!(fs::read_to_string(root.join("picked"))?, "picked\n");
    let undo = visual
        .debug_bounds("git-history-rewrite-undo")
        .unwrap()
        .center();
    visual.simulate_click(undo, Modifiers::default());
    draw(visual);
    assert_eq!(git::head(&root)?.as_deref(), Some(before.as_str()));
    assert!(!root.join("picked").exists());

    let newest = row(visual, 1);
    visual.simulate_click(newest, Modifiers::default());
    visual.simulate_keystrokes("shift-down");
    draw(visual);
    assert!(visual.debug_bounds("git-history-selected-count").is_some());
    assert!(open_menu(visual, 1) > shared_menu);
    visual.simulate_keystrokes("up up enter");
    draw(visual);
    let picked = git::commit(&root, &git::head(&root)?.unwrap())?;
    assert_eq!(picked.subject, "Pick this next");
    let first = git::commit(&root, &picked.parents[0])?;
    assert_eq!(first.subject, "Pick this change");
    assert_eq!(first.parents, [before.clone()]);
    assert_eq!(fs::read_to_string(root.join("picked"))?, "picked again\n");
    let undo = visual
        .debug_bounds("git-history-rewrite-undo")
        .unwrap()
        .center();
    visual.simulate_click(undo, Modifiers::default());
    draw(visual);
    assert_eq!(git::head(&root)?.as_deref(), Some(before.as_str()));
    assert!(!root.join("picked").exists());

    visual.executor().advance_clock(Duration::from_millis(500));
    draw(visual);
    assert!(visual.debug_bounds("git-history-rewrite-undo").is_none());

    fs::write(root.join("base"), "local draft\n")?;
    let newest = row(visual, 1);
    visual.simulate_click(newest, Modifiers::default());
    visual.simulate_keystrokes("shift-down");
    draw(visual);
    assert!(open_menu(visual, 1) > shared_menu);
    visual.simulate_keystrokes("up up enter");
    draw(visual);
    let partial = git::head(&root)?.unwrap();
    let first = git::commit(&root, &partial)?;
    assert_eq!(first.subject, "Pick this change");
    assert_eq!(first.parents, [before.clone()]);
    assert!(!root.join(".git/CHERRY_PICK_HEAD").exists());
    assert!(!root.join(".git/stroll/history-backup").exists());
    assert!(visual.debug_bounds("git-history-rewrite-undo").is_none());
    assert!(visual.debug_bounds("git-history-saved-changes").is_none());
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(panel.history_progress.as_ref().unwrap().stopped_before_pick);
        let error = panel.stage_error.as_deref().unwrap();
        assert!(error.contains("would be overwritten") && error.contains("base"));
    });
    let continue_button = visual
        .debug_bounds("git-history-continue")
        .unwrap()
        .center();
    visual.simulate_click(continue_button, Modifiers::default());
    draw(visual);
    assert_eq!(git::head(&root)?.as_deref(), Some(partial.as_str()));

    let abort = visual.debug_bounds("git-history-abort").unwrap().center();
    visual.simulate_click(abort, Modifiers::default());
    draw(visual);
    let confirm = visual
        .debug_bounds("git-history-edit-apply")
        .unwrap()
        .center();
    visual.simulate_click(confirm, Modifiers::default());
    draw(visual);
    assert_eq!(git::head(&root)?.as_deref(), Some(before.as_str()));
    assert_eq!(fs::read_to_string(root.join("base"))?, "local draft\n");
    assert!(!root.join("picked").exists());
    assert!(!root.join(".git/sequencer").exists());
    assert!(visual.debug_bounds("git-history-edit-progress").is_none());

    let history = git::history(&root)?;
    assert_eq!(history.checkout.as_deref(), Some("refs/heads/main"));
    assert!(
        history
            .branches
            .iter()
            .any(|branch| branch.reference == "refs/heads/source" && branch.oid == source)
    );
    Ok(())
}

#[::core::prelude::v1::test]
fn commit_list_squashes_rewords_and_resets_with_local_changes() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repo");
    fs::create_dir(&root)?;
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let mut commits = Vec::new();
    for (file, title) in [
        ("base", "Base"),
        ("one", "First"),
        ("two", "Second"),
        ("three", "Third"),
    ] {
        fs::write(root.join(file), title)?;
        git(&root, &["add", "--", file]);
        git(&root, &["commit", "-qm", title]);
        commits.push(git::head(&root)?.unwrap());
    }
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let platform = gpui_platform::current_platform(true);
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    let mut workspace = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Git, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(1280.), px(760.)));
    visual.update(|window, cx| {
        cx.set_reduce_motion(true);
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root: root.clone(),
                    file: None,
                },
                window,
                cx,
            );
        });
    });
    let draw = |visual: &mut VisualTestContext| {
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.simulate_next_frame(cx);
            });
        }
    };
    draw(visual);
    let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
    let row = |visual: &mut VisualTestContext, index| {
        visual.update(|_, cx| {
            panel
                .read(cx)
                .history_scroll
                .bounds_for_item(index)
                .unwrap()
                .center()
        })
    };
    let newest = row(visual, 1);
    visual.simulate_click(newest, Modifiers::default());
    draw(visual);
    visual.simulate_keystrokes("shift-down");
    draw(visual);
    assert!(visual.debug_bounds("git-history-selected-count").is_some());
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        let selection = panel.history_pick.rows.as_ref().unwrap();
        assert_eq!(selection.len(), 2);
        assert!(selection.contains(&SharedString::from(commits[3].clone())));
        assert!(selection.contains(&SharedString::from(commits[2].clone())));
        assert_eq!(
            panel.selection.as_ref().and_then(Selection::focused_commit),
            Some(commits[2].as_str())
        );
    });
    let second = row(visual, 2);
    visual.simulate_click(
        second,
        Modifiers {
            control: !cfg!(target_os = "macos"),
            platform: cfg!(target_os = "macos"),
            ..Default::default()
        },
    );
    draw(visual);
    assert!(visual.debug_bounds("git-history-selected-count").is_none());
    visual.simulate_click(
        second,
        Modifiers {
            control: !cfg!(target_os = "macos"),
            platform: cfg!(target_os = "macos"),
            ..Default::default()
        },
    );
    draw(visual);
    visual.simulate_mouse_down(newest, MouseButton::Right, Modifiers::default());
    visual.simulate_mouse_up(newest, MouseButton::Right, Modifiers::default());
    draw(visual);
    visual.simulate_keystrokes("down down enter");
    draw(visual);
    assert!(visual.debug_bounds("git-history-message").is_some());
    visual.simulate_keystrokes("ctrl-enter");
    draw(visual);
    assert!(
        visual.debug_bounds("git-history-message").is_none(),
        "Ctrl+Enter should close the message editor"
    );
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(!panel.staging);
        assert!(panel.stage_error.is_none(), "{:?}", panel.stage_error);
    });
    let squashed = git::head(&root)?.unwrap();
    let commit = git::commit(&root, &squashed)?;
    assert_eq!(commit.parents, [commits[1].clone()]);
    assert_eq!(commit.subject, "Second");
    assert_eq!(commit.body, "Third");

    let renamed = {
        let newest = row(visual, 1);
        visual.simulate_click(newest, Modifiers::default());
        draw(visual);
        visual.simulate_keystrokes("f2");
        draw(visual);
        assert!(visual.debug_bounds("git-history-message").is_some());
        visual.simulate_keystrokes("ctrl-a");
        visual.simulate_input("Squashed changes");
        visual.simulate_keystrokes("ctrl-enter");
        draw(visual);
        let renamed = git::head(&root)?.unwrap();
        assert_ne!(renamed, squashed);
        assert_eq!(git::commit(&root, &renamed)?.subject, "Squashed changes");
        renamed
    };

    let newest = row(visual, 1);
    let oldest = row(visual, 3);
    visual.simulate_mouse_down(oldest, MouseButton::Right, Modifiers::default());
    visual.simulate_mouse_up(oldest, MouseButton::Right, Modifiers::default());
    draw(visual);
    visual.simulate_keystrokes("down down down enter");
    draw(visual);
    let selector = |oid: &str| -> &'static str {
        Box::leak(format!("git-history-plan-row:{oid}").into_boxed_str())
    };
    let base_row = selector(&commits[0]);
    let first_row = selector(&commits[1]);
    let renamed_row = selector(&renamed);
    let y = |visual: &mut VisualTestContext, selector| visual.debug_bounds(selector).unwrap().top();
    assert!(y(visual, base_row) < y(visual, first_row));
    assert!(y(visual, first_row) < y(visual, renamed_row));
    visual.simulate_keystrokes("alt-down");
    draw(visual);
    assert!(y(visual, first_row) < y(visual, base_row));
    let trigger = visual.debug_bounds("git-history-plan-action:0").unwrap();
    visual.simulate_click(trigger.center(), Modifiers::default());
    draw(visual);
    let surface = visual.debug_bounds("popup-menu-surface").unwrap();
    let dialog = visual.debug_bounds("dialog-0").unwrap();
    let left = surface.left().max(dialog.left());
    let right = surface.right().min(dialog.right());
    let reword = point(
        (left + right) / 2.,
        surface.top() + surface.size.height * 0.3,
    );
    assert!(
        right > left && dialog.contains(&reword),
        "Reword must overlap the dialog surface: menu {surface:?}, dialog {dialog:?}"
    );
    visual.simulate_click(reword, Modifiers::default());
    draw(visual);
    assert!(visual.debug_bounds("git-history-plan-message").is_some());
    visual.simulate_keystrokes("escape");
    draw(visual);

    // Noncontiguous squash uses the same prefilled message editor as adjacent squash.
    visual.simulate_click(newest, Modifiers::default());
    visual.simulate_click(
        oldest,
        Modifiers {
            control: true,
            ..Default::default()
        },
    );
    draw(visual);
    visual.simulate_mouse_down(newest, MouseButton::Right, Modifiers::default());
    visual.simulate_mouse_up(newest, MouseButton::Right, Modifiers::default());
    draw(visual);
    visual.simulate_keystrokes("down down enter");
    draw(visual);
    assert!(visual.debug_bounds("git-history-message").is_some());
    assert!(visual.debug_bounds(base_row).is_none());
    visual.simulate_keystrokes("ctrl-end enter enter");
    visual.simulate_input("Reviewed");
    visual.simulate_keystrokes("ctrl-enter");
    draw(visual);
    let final_tip = git::commit(&root, &git::head(&root)?.unwrap())?;
    assert_eq!(final_tip.subject, "First");
    let root_commit = git::commit(&root, &final_tip.parents[0])?;
    assert_eq!(root_commit.subject, "Base");
    assert_eq!(root_commit.body, "Squashed changes\n\nReviewed");
    assert!(root_commit.parents.is_empty());

    // Reset is a single-selection action. Inspect Hard, then apply the default Soft mode.
    let open_reset = |visual: &mut VisualTestContext| {
        let oldest = row(visual, 2);
        visual.simulate_mouse_down(oldest, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(oldest, MouseButton::Right, Modifiers::default());
        draw(visual);
        visual.simulate_keystrokes("down down down down down down enter");
        draw(visual);
        assert!(visual.debug_bounds("git-reset-dialog").is_some());
    };
    let apply = |visual: &mut VisualTestContext| {
        let button = visual
            .debug_bounds("git-history-edit-apply")
            .unwrap()
            .center();
        visual.simulate_click(button, Modifiers::default());
        draw(visual);
    };
    open_reset(visual);
    let hard = visual.debug_bounds("git-reset-hard").unwrap().center();
    visual.simulate_click(hard, Modifiers::default());
    draw(visual);
    assert!(visual.debug_bounds("git-reset-hard-warning").is_some());
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(visual.debug_bounds("git-reset-dialog").is_none());

    fs::write(root.join("two"), "staged before reset")?;
    git(&root, &["add", "two"]);
    fs::write(root.join("two"), "unstaged before reset")?;
    fs::write(root.join("untracked"), "untracked before reset")?;
    open_reset(visual);
    assert!(visual.debug_bounds("git-reset-hard-warning").is_none());
    apply(visual);
    assert_eq!(git::head(&root)?.as_deref(), Some(root_commit.oid.as_str()));
    let status = git::history(&root)?.changes;
    assert!(status["two"].staged && status["two"].unstaged);
    assert!(status["one"].staged);
    assert!(status["untracked"].untracked);

    let head = row(visual, 1);
    visual.simulate_click(head, Modifiers::default());
    draw(visual);
    visual.simulate_keystrokes("f2");
    draw(visual);
    assert!(visual.debug_bounds("git-history-message").is_some());
    visual.simulate_keystrokes("ctrl-a");
    visual.simulate_input("Root message with a dirty worktree");
    visual.simulate_keystrokes("ctrl-enter");
    draw(visual);
    let commit = git::commit(&root, &git::head(&root)?.unwrap())?;
    assert_eq!(commit.subject, "Root message with a dirty worktree");
    assert_eq!(git::history(&root)?.changes, status);
    assert_eq!(
        fs::read_to_string(root.join("two"))?,
        "unstaged before reset"
    );
    assert_eq!(
        fs::read_to_string(root.join("untracked"))?,
        "untracked before reset"
    );
    assert!(visual.update(|_, cx| panel.read(cx).history_rewrite.is_none()));
    let before_amend = commit.oid;
    let remote = "refs/remotes/origin/main";
    git(&root, &["update-ref", remote, &before_amend]);
    let worktree = row(visual, 0);
    visual.simulate_click(worktree, Modifiers::default());
    draw(visual);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.commit_message.update(cx, |input, cx| {
                input.set_value("Ordinary commit draft", window, cx);
            });
        });
    });
    draw(visual);
    let toggle_amend = |visual: &mut VisualTestContext| {
        let split = visual.debug_bounds("git-commit-split").unwrap();
        let button = visual.debug_bounds("git-create-commit").unwrap();
        let dropdown = visual.debug_bounds("dropdown-button-trigger").unwrap();
        let small_button_height = visual.update(|window, _| rems(1.5).to_pixels(window.rem_size()));
        assert!((button.size.height - small_button_height).abs() < px(1.));
        assert_eq!(button.size.height, split.size.height);
        assert_eq!(dropdown.top(), button.top());
        assert_eq!(dropdown.bottom(), button.bottom());
        assert_eq!(dropdown.left(), button.right());
        visual.simulate_click(dropdown.center(), Modifiers::default());
        draw(visual);
        visual.simulate_keystrokes("down enter");
        draw(visual);
    };
    let message = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            (
                panel.commit_amend.is_some(),
                panel.commit_message.read(cx).value(),
            )
        })
    };
    toggle_amend(visual);
    assert_eq!(
        message(visual),
        (true, "Root message with a dirty worktree".into())
    );
    visual.update(|_, cx| {
        assert_eq!(
            panel.read(cx).commit_draft(cx).as_str(),
            "Ordinary commit draft"
        );
    });
    assert!(visual.debug_bounds("git-amend-message").is_none());
    assert_eq!(git::head(&root)?.as_deref(), Some(before_amend.as_str()));
    toggle_amend(visual);
    assert_eq!(message(visual), (false, "Ordinary commit draft".into()));
    toggle_amend(visual);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.commit_message.update(cx, |input, cx| {
                input.set_value("Amended from the worktree", window, cx);
            });
        });
    });
    draw(visual);
    let button = visual.debug_bounds("git-create-commit").unwrap().center();
    visual.simulate_click(button, Modifiers::default());
    draw(visual);
    assert!(
        visual
            .debug_bounds("git-history-published-confirmation")
            .is_some()
    );
    assert_eq!(git::head(&root)?.as_deref(), Some(before_amend.as_str()));
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(
        visual
            .debug_bounds("git-history-published-confirmation")
            .is_none()
    );
    assert_eq!(message(visual), (true, "Amended from the worktree".into()));
    assert_eq!(git::head(&root)?.as_deref(), Some(before_amend.as_str()));
    visual.simulate_click(button, Modifiers::default());
    draw(visual);
    let confirm = visual
        .debug_bounds("git-history-edit-apply")
        .unwrap()
        .center();
    visual.simulate_click(confirm, Modifiers::default());
    draw(visual);
    let amended = git::commit(&root, &git::head(&root)?.unwrap())?;
    assert_ne!(amended.oid, before_amend);
    assert_eq!(
        git::history(&root)?
            .branches
            .iter()
            .find(|branch| branch.reference == remote)
            .unwrap()
            .oid,
        before_amend
    );
    assert_eq!(amended.subject, "Amended from the worktree");
    assert_eq!(message(visual), (false, "Ordinary commit draft".into()));
    assert!(
        git::history(&root)?
            .changes
            .values()
            .all(|file| !file.staged)
    );
    let undo = visual.debug_bounds("git-history-rewrite-undo").unwrap();
    visual.simulate_click(undo.center(), Modifiers::default());
    draw(visual);
    assert_eq!(git::head(&root)?.as_deref(), Some(before_amend.as_str()));
    assert_eq!(git::history(&root)?.changes, status);
    Ok(())
}
