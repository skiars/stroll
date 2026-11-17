use std::path::Path;

use stroll::docs::{Destination, destination};

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
