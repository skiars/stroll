#[cfg(target_os = "linux")]
#[gpui::test]
fn markdown_image_clicks_use_the_image_handler(cx: &mut gpui::TestAppContext) {
    use std::sync::{Arc, Mutex};

    use gpui::{
        AppContext as _, Context, Entity, Render, SharedString, Styled as _, Window, point, px,
    };
    use gpui_component::{
        Root,
        text::{TextView, TextViewState},
    };

    struct Images {
        text: Entity<TextViewState>,
        images: Arc<Mutex<Vec<SharedString>>>,
        links: Arc<Mutex<Vec<SharedString>>>,
    }

    impl Render for Images {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            let images = self.images.clone();
            let links = self.links.clone();
            TextView::new(&self.text)
                .w(px(260.))
                .on_image_click(move |url, _, _, _| {
                    images.lock().unwrap().push(url.clone());
                })
                .on_link_click(move |url, _, _, _| {
                    links.lock().unwrap().push(url.clone());
                })
        }
    }

    let images = Arc::new(Mutex::new(Vec::new()));
    let links = Arc::new(Mutex::new(Vec::new()));
    let captured_images = images.clone();
    let captured_links = links.clone();
    let text = std::rc::Rc::new(std::cell::RefCell::new(None));
    let text_handle = text.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        gpui_component::init(cx);
        let state = cx.new(|cx| {
            TextViewState::markdown(
                r#"Before [<img src="sample.png" width="80" height="60">](https://example.com/image-link) after."#,
                cx,
            )
        });
        *text_handle.borrow_mut() = Some(state.clone());
        Root::new(
            cx.new(|_| Images {
                text: state,
                images: captured_images,
                links: captured_links,
            }),
            window,
            cx,
        )
    });
    let cx: &mut gpui::VisualTestContext = cx;
    let text = text.borrow().clone().unwrap();
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let bounds = text.read_with(cx, |text, _| text.bounds());
    cx.simulate_click(
        point(bounds.left() + px(70.), bounds.top() + px(30.)),
        Default::default(),
    );

    assert_eq!(
        *images.lock().unwrap(),
        vec![SharedString::from("sample.png")],
        "the image handler should receive the image URL"
    );
    assert!(
        links.lock().unwrap().is_empty(),
        "an image click must not follow the wrapped link"
    );
    assert_eq!(cx.opened_url(), None);
}

#[cfg(target_os = "linux")]
#[test]
fn gpu_image_sampling_preserves_texels_only_with_nearest_neighbor() -> anyhow::Result<()> {
    use std::sync::Arc;

    use gpui::{
        AppContext as _, Context, HeadlessAppContext, ImageSampling, IntoElement, ObjectFit,
        ParentElement as _, Render, RenderImage, Styled as _, StyledImage as _, Window, div, img,
        px, size,
    };
    use image::{Frame, Rgba, RgbaImage};

    struct SamplingProbe(Arc<RenderImage>);

    impl Render for SamplingProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .size_full()
                .child(
                    img(self.0.clone())
                        .size_full()
                        .object_fit(ObjectFit::Fill)
                        .w(px(63.5)),
                )
                .child(
                    img(self.0.clone())
                        .size_full()
                        .object_fit(ObjectFit::Fill)
                        .image_sampling(ImageSampling::Nearest)
                        .w(px(63.5)),
                )
        }
    }

    let source = Arc::new(RenderImage::new([Frame::new(RgbaImage::from_fn(
        2,
        1,
        |x, _| {
            let channel = if x == 0 { 0 } else { 255 };
            Rgba([channel, channel, channel, 255])
        },
    ))]));
    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(()),
        || Ok(Some(Box::new(gpui_wgpu::WgpuHeadlessRenderer::new()?))),
    );
    let window = cx.open_window(size(px(127.), px(30.)), |_, cx| {
        cx.new(|_| SamplingProbe(source))
    })?;
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    let image = cx.capture_screenshot(window.into())?;
    let width = image.width() / 2;
    let y = image.height() / 2;
    let edge = width / 2;
    let pixel = |x| image.get_pixel(x, y).0[0];

    assert!(
        (32..224).contains(&pixel(edge)),
        "linear interpolation blends at the boundary"
    );
    assert_eq!(pixel(width + edge - 1), 0);
    assert_eq!(pixel(width + edge + 1), 255);
    assert!(
        (width..image.width()).all(|x| matches!(pixel(x), 0 | 255)),
        "nearest-neighbor output contains only source texels",
    );
    if let Some(path) = std::env::var_os("STROLL_IMAGE_SAMPLING_SCREENSHOT") {
        image.save(path)?;
    }
    Ok(())
}
