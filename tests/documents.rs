use std::path::Path;

use stroll::docs::{Destination, destination};

#[cfg(target_os = "linux")]
#[gpui::test]
fn markdown_checklist_items_keep_the_body_text_color(cx: &mut gpui::TestAppContext) {
    use gpui::{AppContext as _, Context, Entity, Render, Window};
    use gpui_component::text::{TextView, TextViewState};

    struct Checklist(Entity<TextViewState>);
    impl Render for Checklist {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            TextView::new(&self.0)
        }
    }

    cx.update(gpui_component::init);
    let (_, visual) = cx.add_window_view(|_, cx| {
        let state = cx.new(|cx| {
            TextViewState::markdown("<u>Body</u>\n\n- [ ] <u>Open</u>\n- [x] <u>Done</u>", cx)
        });
        Checklist(state)
    });
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        // Underlines without an explicit color follow the color of the glyphs.
        let mut lines = window.painted_underlines();
        lines.sort_by(|a, b| a.bounds.top().0.total_cmp(&b.bounds.top().0));
        let [body, open, done] = lines.as_slice() else {
            panic!("expected three underlined text runs");
        };
        assert_eq!(open.color, body.color);
        assert_eq!(done.color, body.color);
    });
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn markdown_link_preview_waits_for_pointer_rest_and_stays_in_the_window(
    cx: &mut gpui::TestAppContext,
) {
    use std::time::Duration;

    use gpui::{
        AppContext as _, Context, Entity, InteractiveElement as _, ParentElement as _, Render,
        Styled as _, Window, div, point, px, size,
    };
    use gpui_component::{
        text::{TextView, TextViewState},
        tooltip::Tooltip,
    };

    const EXTERNAL: &str = "https://example.com/releases/desktop/2026/11/build-report.html?machine=linux-x86_64&status=passed";
    const RELATIVE: &str = "./next.md#section";

    struct Preview {
        text: Entity<TextViewState>,
        covered: bool,
    }
    impl Render for Preview {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            div()
                .relative()
                .size_full()
                .child(
                    TextView::new(&self.text)
                        .absolute()
                        .left(px(136.))
                        .top(px(152.))
                        .w(px(180.))
                        .text_size(px(16.))
                        .link_tooltip(|url, window, cx| {
                            let url = url.clone();
                            Tooltip::element(move |_, _| {
                                div()
                                    .debug_selector(|| match url.as_ref() {
                                        EXTERNAL => "external-link-preview".into(),
                                        RELATIVE => "relative-link-preview".into(),
                                        _ => panic!("unexpected preview URL: {url}"),
                                    })
                                    .child(url.clone())
                            })
                            .build(window, cx)
                        }),
                )
                .children(self.covered.then(|| div().absolute().inset_0().occlude()))
        }
    }

    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let (root, visual) = cx.add_window_view(|_, cx| Preview {
        text: cx.new(|cx| {
            TextViewState::markdown(&format!("[First]({EXTERNAL}) and [Second]({RELATIVE})"), cx)
        }),
        covered: false,
    });
    visual.simulate_resize(size(px(320.), px(190.)));
    let draw = |visual: &mut gpui::VisualTestContext, millis| {
        visual.run_until_parked();
        visual
            .executor()
            .advance_clock(Duration::from_millis(millis));
        for _ in 0..2 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
    };
    draw(visual, 0);

    let first = point(px(141.), px(161.));
    let second = point(px(245.), first.y);
    visual.simulate_mouse_move(first, None, Default::default());
    draw(visual, 250);
    assert!(visual.debug_bounds("external-link-preview").is_none());
    visual.simulate_mouse_move(first + point(px(4.), px(0.)), None, Default::default());
    draw(visual, 299);
    assert!(visual.debug_bounds("external-link-preview").is_none());
    draw(visual, 1);
    let popup = visual
        .debug_bounds("external-link-preview")
        .expect("the current link's address appears after 300 ms");
    assert!(popup.size.width >= px(160.), "{popup:?}");
    assert!(
        popup.left() >= px(0.)
            && popup.top() >= px(0.)
            && popup.right() <= px(320.)
            && popup.bottom() <= px(190.),
        "{popup:?}"
    );

    visual.simulate_mouse_move(second, None, Default::default());
    draw(visual, 299);
    assert!(visual.debug_bounds("external-link-preview").is_none());
    assert!(visual.debug_bounds("relative-link-preview").is_none());
    draw(visual, 1);
    let popup = visual.debug_bounds("relative-link-preview").unwrap();
    assert!(popup.right() <= px(320.), "{popup:?}");

    for covered in [true, false] {
        root.update(visual, |root, cx| {
            root.covered = covered;
            cx.notify();
        });
        draw(visual, 0);
        assert!(visual.debug_bounds("relative-link-preview").is_none());
    }

    visual.simulate_mouse_move(second + point(px(2.), px(0.)), None, Default::default());
    draw(visual, 300);
    assert!(visual.debug_bounds("relative-link-preview").is_some());
    visual.simulate_mouse_move(point(px(20.), px(20.)), None, Default::default());
    draw(visual, 0);
    assert!(visual.debug_bounds("relative-link-preview").is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn clipped_image_does_not_enter_the_atlas_until_visible() -> anyhow::Result<()> {
    use std::sync::Arc;

    use gpui::{
        AppContext as _, Bounds, ContentMask, Context, Corners, DevicePixels, HeadlessAppContext,
        HeadlessAtlas, PlatformAtlas, PlatformHeadlessRenderer, Render, RenderImage,
        RenderImageParams, Scene, Size, Styled as _, Window, canvas, point, px, size,
    };
    use image::{Frame, Rgba, RgbaImage};

    struct Renderer(Arc<HeadlessAtlas>);
    impl PlatformHeadlessRenderer for Renderer {
        fn render_scene_to_image(
            &mut self,
            _: &Scene,
            _: Size<DevicePixels>,
        ) -> anyhow::Result<RgbaImage> {
            anyhow::bail!("This test only checks atlas allocation")
        }

        fn render_scene(&mut self, _: &Scene, _: Size<DevicePixels>) -> anyhow::Result<()> {
            Ok(())
        }

        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.0.clone()
        }
    }

    struct Preview {
        image: Arc<RenderImage>,
        top: gpui::Pixels,
    }
    impl Render for Preview {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            let image = self.image.clone();
            let top = self.top;
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let bounds = Bounds::new(point(px(10.), top), size(px(80.), px(80.)));
                    window.with_content_mask(
                        Some(ContentMask {
                            bounds: Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.))),
                        }),
                        |window| {
                            window
                                .paint_image(bounds, bounds, Corners::all(px(0.)), image, 0, false)
                                .unwrap();
                        },
                    );
                },
            )
            .size_full()
        }
    }

    let image = Arc::new(RenderImage::new([Frame::new(RgbaImage::from_pixel(
        10,
        10,
        Rgba([255, 0, 0, 255]),
    ))]));
    let key = RenderImageParams {
        image_id: image.id,
        frame_index: 0,
    }
    .into();
    let atlas = Arc::new(HeadlessAtlas::default());
    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(()),
        {
            let atlas = atlas.clone();
            move || Ok(Some(Box::new(Renderer(atlas.clone()))))
        },
    );
    let window = cx.open_window(size(px(120.), px(220.)), |_, cx| {
        cx.new(|_| Preview {
            image,
            top: px(110.),
        })
    })?;
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    assert!(!atlas.contains(&key));

    cx.update_window(window.into(), |root, window, cx| {
        root.downcast::<Preview>()
            .unwrap()
            .update(cx, |preview, cx| {
                preview.top = px(90.);
                cx.notify();
            });
        window.draw(cx).clear(cx);
    })?;
    assert!(atlas.contains(&key));
    Ok(())
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn html_table_headers_follow_the_markup_and_gfm_keeps_its_header(cx: &mut gpui::TestAppContext) {
    use std::sync::{Arc, Mutex};

    use gpui::{
        AppContext as _, Background, Context, Empty, Entity, ParentElement as _, Render,
        StyleRefinement, Styled as _, Window, div, px, rgb, size,
    };
    use gpui_component::text::{TableData, TextView, TextViewState, TextViewStyle};

    const HEADER_FILL: u32 = 0x11aa77;
    const SOURCE: &str =
        "<table><tbody><tr><td><strong>Card title</strong></td><td>Image B</td></tr>
<tr><td>Image C</td><td>Image D</td></tr></tbody></table>

<table><thead><tr><th>Column</th><th></th></tr></thead>
<tbody><tr><th scope='row'>Row</th><td>Value</td></tr></tbody></table>

| First | Second |
| --- | --- |
| One | Two |
";

    struct Tables {
        state: Entity<TextViewState>,
        data: Arc<Mutex<Vec<TableData>>>,
    }
    impl Render for Tables {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            let data = self.data.clone();
            div().size_full().child(
                TextView::new(&self.state)
                    .style(
                        TextViewStyle::default()
                            .table_head(StyleRefinement::default().bg(rgb(HEADER_FILL))),
                    )
                    .table_actions(move |table, _, _| {
                        data.lock().unwrap().push(table.clone());
                        Empty
                    }),
            )
        }
    }

    cx.update(gpui_component::init);
    let data = Arc::new(Mutex::new(Vec::new()));
    let (_, visual) = cx.add_window_view(|_, cx| Tables {
        state: cx.new(|cx| TextViewState::markdown(SOURCE, cx)),
        data: data.clone(),
    });
    visual.simulate_resize(size(px(750.), px(550.)));
    visual.run_until_parked();
    data.lock().unwrap().clear();
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        let header: Background = rgb(HEADER_FILL).into();
        assert_eq!(
            window
                .painted_quads()
                .iter()
                .filter(|quad| quad.background == header)
                .count(),
            2,
            "Only the HTML head and GFM header rows get the header fill"
        );
    });
    let tables = data.lock().unwrap();
    assert_eq!(tables.len(), 3);
    assert!(tables[0].headers.is_empty());
    assert_eq!(tables[0].rows[0], ["Card title", "Image B"]);
    assert!(tables[0].markdown.starts_with("<table><tbody><tr><td>"));
    assert_eq!(tables[1].headers, ["Column", ""]);
    assert_eq!(tables[1].rows, [vec!["Row", "Value"]]);
    assert!(tables[1].markdown.contains("<tbody><tr><th>"));
    assert_eq!(tables[2].headers, ["First", "Second"]);
    assert_eq!(tables[2].rows, [vec!["One", "Two"]]);
}

#[test]
fn resolves_repository_paths_and_rejects_escapes() {
    let path = Path::new("docs/intro.md");
    assert_eq!(
        destination(path, "../%E8%AF%B4%E6%98%8E.md#%E5%BC%80%E5%A7%8B"),
        Some(Destination::Local {
            path: "说明.md".into(),
            fragment: Some("开始".into()),
        })
    );
    assert_eq!(
        destination(path, "/README.md").unwrap(),
        Destination::Local {
            path: "README.md".into(),
            fragment: None,
        }
    );
    assert!(destination(path, "../../outside.md").is_none());
    assert!(destination(path, "file:///etc/passwd").is_none());
    assert!(destination(path, "javascript:alert(1)").is_none());
}

#[test]
fn loads_markdown_regardless_of_repository_membership_or_visibility() -> anyhow::Result<()> {
    use std::{fs, process::Command};

    use stroll::repository::Repository;

    let dir = tempfile::tempdir()?;
    let root = dir.path().join("repo");
    fs::create_dir(&root)?;
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()?
            .success()
    );
    fs::write(root.join("visible.md"), "# Visible\n")?;
    fs::write(root.join(".gitignore"), "ignored.md\n")?;
    fs::write(root.join("ignored.md"), "# Ignored\n")?;
    fs::write(root.join(".hidden.md"), "# Hidden\n")?;
    fs::write(dir.path().join("outside.md"), "# Outside\n")?;
    let repo = Repository::open(&root)?;

    let load = |path: &Path| stroll::docs::load(&repo, path);
    assert!(load(Path::new("visible.md")).is_ok());
    assert!(load(Path::new("ignored.md")).is_ok());
    assert!(load(Path::new(".hidden.md")).is_ok());
    assert!(load(Path::new("../outside.md")).is_ok());
    assert!(load(&dir.path().join("outside.md")).is_ok());
    assert!(load(Path::new("README.txt")).is_err());
    Ok(())
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn html_table_places_caption_paragraphs_below_the_image(cx: &mut gpui::TestAppContext) {
    use std::sync::Arc;

    use gpui::{
        AppContext as _, Context, Entity, ImageSource, Render, RenderImage, Styled as _, Window, px,
    };
    use gpui_component::{
        Root,
        text::{TextView, TextViewState},
    };
    use image::{Frame, Rgba, RgbaImage};

    const GALLERY: &str = r##"<table><tr>
      <td>
        <a href="#diagram"><img src="sample.png" width="100" height="80"></a>
        <p><u>Overview</u></p>
        <div><p><u>Details</u></p></div>
      </td>
      <td><u>Reference</u></td>
    </tr></table>"##;

    struct Gallery {
        text: Entity<TextViewState>,
        image: Arc<RenderImage>,
    }
    impl Render for Gallery {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            let image = self.image.clone();
            TextView::new(&self.text)
                .w(px(500.))
                .image_source(move |_| ImageSource::Render(image.clone()))
        }
    }

    let (_, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let gallery = cx.new(|cx| Gallery {
            text: cx.new(|cx| TextViewState::markdown(GALLERY, cx)),
            image: Arc::new(RenderImage::new([Frame::new(RgbaImage::from_pixel(
                100,
                80,
                Rgba([90, 150, 40, 255]),
            ))])),
        });
        Root::new(gallery, window, cx)
    });
    cx.run_until_parked();
    let mut lines = cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window
            .painted_underlines()
            .into_iter()
            .map(|line| line.bounds.map(|value| px(value.0 / window.scale_factor())))
            .collect::<Vec<_>>()
    });
    // Underlines give the painted caption positions without a screenshot or GPU.
    lines.sort_by(|a, b| a.top().partial_cmp(&b.top()).unwrap());
    let [reference, overview, details] = lines.as_slice() else {
        panic!("expected three underlined text runs, got {lines:?}");
    };
    assert!(reference.left() > overview.left(), "{lines:?}");
    assert!(overview.top() >= reference.top() + px(75.), "{lines:?}");
    assert!(details.top() > overview.top(), "{lines:?}");
    assert!(
        (details.left() - overview.left()).abs() < px(1.),
        "{lines:?}"
    );
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn markdown_selection_stops_on_move_without_left_button(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, Context, Entity, MouseButton, Render, Styled as _, Window, point, px,
    };
    use gpui_component::{
        Root,
        text::{TextView, TextViewState},
    };

    struct Preview(Entity<TextViewState>);
    impl Render for Preview {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            TextView::new(&self.0).scrollable(true).size_full()
        }
    }

    let mut text = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let state = cx.new(|cx| TextViewState::markdown("Alpha bravo charlie delta echo.", cx));
        text = Some(state.clone());
        let preview = cx.new(|_| Preview(state));
        Root::new(preview, window, cx)
    });
    let text = text.unwrap();
    let selection = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let state = text.read(cx);
            (state.selected_text(), state.is_selecting())
        })
    };
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_mouse_down(
        point(px(5.), px(10.)),
        MouseButton::Left,
        Default::default(),
    );
    cx.simulate_mouse_move(
        point(px(65.), px(10.)),
        MouseButton::Left,
        Default::default(),
    );
    let (selected, dragging) = selection(cx);
    assert!(dragging && !selected.is_empty());

    cx.simulate_mouse_move(point(px(185.), px(10.)), None, Default::default());
    assert_eq!(selection(cx), (selected.clone(), false));
    // A later held move from another gesture must not resume this selection.
    cx.simulate_mouse_move(
        point(px(210.), px(10.)),
        MouseButton::Left,
        Default::default(),
    );
    assert_eq!(selection(cx), (selected, false));
}

#[cfg(target_os = "linux")]
#[test]
fn document_thumbnail_uses_the_fitted_size_and_keeps_the_original_for_preview() -> anyhow::Result<()>
{
    use std::sync::{Arc, Mutex};

    use gpui::{
        AppContext as _, Context, DevicePixels, Entity, HeadlessAppContext, HeadlessAtlas,
        ImageSource, ImageThumbnails, ParentElement as _, PlatformAtlas, PlatformHeadlessRenderer,
        Render, RenderImage, Scene, Size, Styled as _, Window, div, img, px, size,
    };
    use image::{Frame, Rgba, RgbaImage};

    type Painted = Arc<Mutex<Vec<Size<DevicePixels>>>>;
    struct Renderer(Arc<HeadlessAtlas>, Painted);
    impl PlatformHeadlessRenderer for Renderer {
        fn render_scene_to_image(
            &mut self,
            scene: &Scene,
            _: Size<DevicePixels>,
        ) -> anyhow::Result<RgbaImage> {
            *self.1.lock().unwrap() = scene
                .polychrome_sprites
                .iter()
                .map(|sprite| sprite.tile.bounds.size)
                .collect();
            Ok(RgbaImage::new(1, 1))
        }

        fn render_scene(&mut self, _: &Scene, _: Size<DevicePixels>) -> anyhow::Result<()> {
            Ok(())
        }

        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.0.clone()
        }
    }

    struct Reader {
        original: Arc<RenderImage>,
        thumbnails: Entity<ImageThumbnails>,
        preview: bool,
    }
    impl Render for Reader {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            let original = ImageSource::Render(self.original.clone());
            let source = if self.preview {
                original
            } else {
                original.fit_to_display(&self.thumbnails)
            };
            div().size_full().child(img(source).w(px(64.)))
        }
    }

    let painted = Painted::default();
    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(()),
        {
            let painted = painted.clone();
            move || {
                Ok(Some(Box::new(Renderer(
                    Arc::new(HeadlessAtlas::default()),
                    painted.clone(),
                ))))
            }
        },
    );
    let window = cx.open_window(size(px(160.), px(140.)), |_, cx| {
        let thumbnails = ImageThumbnails::new(cx);
        cx.new(|_| Reader {
            original: Arc::new(RenderImage::new([Frame::new(RgbaImage::from_pixel(
                256,
                128,
                Rgba([0, 0, 255, 255]),
            ))])),
            thumbnails,
            preview: false,
        })
    })?;
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    let fitted = cx.update_window(window.into(), |_, window, _| {
        size(px(64.), px(32.)).to_device_pixels(window.scale_factor())
    })?;
    cx.capture_screenshot(window.into())?;
    assert_eq!(*painted.lock().unwrap(), [fitted]);

    cx.update_window(window.into(), |root, window, cx| {
        root.downcast::<Reader>().unwrap().update(cx, |reader, cx| {
            reader.preview = true;
            cx.notify();
        });
        window.draw(cx).clear(cx);
    })?;
    cx.capture_screenshot(window.into())?;
    assert_eq!(
        *painted.lock().unwrap(),
        [size(DevicePixels(256), DevicePixels(128))]
    );
    Ok(())
}

#[gpui::test]
fn first_markdown_parse_uses_initial_extensions(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    use gpui_component::text::{MarkdownExtensions, TextViewState};

    let source = "---\n- example\n---\n\n# Body";
    let state = cx.update(|cx| {
        cx.new(|cx| {
            TextViewState::markdown_with_extensions(
                source,
                MarkdownExtensions::default().frontmatter(),
                cx,
            )
        })
    });

    // Inspect the synchronous parse before the first layout can change the extensions.
    state.read_with(cx, |state, _| {
        assert_eq!(state.source_offset_for_block(0), Some(0));
        assert_eq!(state.source_offset_for_block(1), source.find("# Body"));
        assert_eq!(state.source_offset_for_block(2), None);
    });
}

#[cfg(target_os = "linux")]
#[test]
fn document_images_use_intrinsic_size_unless_html_sets_dimensions() -> anyhow::Result<()> {
    use std::sync::Arc;

    use gpui::{
        AppContext as _, Context, HeadlessAppContext, ParentElement as _, Render, RenderImage,
        SharedString, Styled as _, Window, div, px, size,
    };
    use gpui_component::text::TextView;
    use gpui_wgpu::WgpuHeadlessRenderer;
    use image::{Frame, Rgba, RgbaImage};

    struct Reader {
        markdown: SharedString,
        width: f32,
        image: Arc<RenderImage>,
    }

    impl Render for Reader {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            let image = self.image.clone();
            div().w(px(self.width)).child(
                TextView::markdown("image-sizing", self.markdown.clone())
                    .w_full()
                    .image_source(move |_| image.clone().into()),
            )
        }
    }

    let mut app = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
    );
    app.update(gpui_component::init);
    let handle = app.open_window(size(px(420.), px(240.)), |_, cx| {
        cx.new(|_| Reader {
            markdown: SharedString::default(),
            width: 420.,
            image: Arc::new(RenderImage::new([Frame::new(RgbaImage::from_pixel(
                100,
                50,
                Rgba([0, 0, 255, 255]),
            ))])),
        })
    })?;

    for (markdown, width, scale, expected) in [
        ("![sample](image.png)", 420., 1., (100, 50)),
        // Keep the text and layout width fixed while changing the display scale.
        ("![sample](image.png)", 420., 2., (100, 50)),
        ("Text ![sample](image.png)", 420., 1.5, (100, 50)),
        ("[![sample](image.png)](page.md)", 420., 1., (100, 50)),
        ("<img src=\"image.png\">", 420., 2., (100, 50)),
        ("<img src=\"image.png\" width=\"80\">", 420., 2., (160, 80)),
        (
            "<img src=\"image.png\" width=\"300\">",
            420.,
            1.,
            (300, 150),
        ),
        ("<img src=\"image.png\" height=\"80\">", 420., 1., (160, 80)),
        (
            "<img src=\"image.png\" width=\"300\" height=\"120\">",
            420.,
            1.,
            (240, 120),
        ),
        (
            "<img src=\"image.png\" style=\"width: 300px\">",
            240.,
            1.,
            (240, 120),
        ),
        (
            "<img src=\"image.png\" width=\"50%\">",
            420.,
            1.,
            (210, 105),
        ),
        ("![sample](image.png)", 80., 1., (80, 40)),
        ("![sample](image.png)", 40., 2., (80, 40)),
    ] {
        app.update_window(handle.into(), |root, window, cx| {
            window.set_scale_factor(scale);
            root.downcast::<Reader>().unwrap().update(cx, |reader, cx| {
                reader.markdown = markdown.into();
                reader.width = width;
                cx.notify();
            });
        })?;
        for _ in 0..3 {
            app.run_until_parked();
            app.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
        }
        let screenshot = app.capture_screenshot(handle.into())?;
        let (min_x, min_y, max_x, max_y) = screenshot
            .enumerate_pixels()
            .filter(|(_, _, pixel)| u16::from(pixel[0]) > u16::from(pixel[1].max(pixel[2])) + 32)
            .fold(
                (u32::MAX, u32::MAX, 0, 0),
                |(min_x, min_y, max_x, max_y), (x, y, _)| {
                    (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y))
                },
            );
        assert!(min_x <= max_x, "image was painted for {markdown}");
        let actual = (max_x - min_x + 1, max_y - min_y + 1);
        assert!(
            actual.0.abs_diff(expected.0) <= 2 && actual.1.abs_diff(expected.1) <= 2,
            "{markdown} at {scale}×: painted {actual:?}, expected {expected:?}",
        );
        let image_center = (min_x + max_x + 1) as f32 / 2.;
        let column_center = width * scale / 2.;
        if markdown.starts_with("Text ") {
            assert!(
                image_center < column_center,
                "inline image follows its text"
            );
        } else {
            assert!(
                (image_center - column_center).abs() <= 2.,
                "{markdown} at {scale}×: image center {image_center}, expected {column_center}",
            );
        }
    }
    Ok(())
}
