use gpui::{
    AnyElement, App, AppContext, Context, DevicePixels, ImageSource, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement, Pixels, Render, RenderImage, SharedString, Size,
    Stateful, Styled, StyledImage as _, SvgRenderer, SvgSize, Task, WeakEntity, Window, div, img,
    px, size,
};
use gpui_component::{
    Placement,
    tooltip::{ManagedTooltipExt as _, TooltipRequest},
};
use rust_i18n::t;
use std::{
    cell::{OnceCell, RefCell},
    io::Cursor,
    path::Path,
    rc::Rc,
    sync::Arc,
};

use super::{
    Workspace,
    hover_info::{ArrowHover, Card, FileInfo},
};
use crate::image_viewer::{self, Preview};

impl Workspace {
    pub(crate) fn open_image_preview(
        &mut self,
        source: ImageSource,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.image_preview.is_none() {
            self.image_preview_return_focus = window.focused(cx);
        }
        self.image_preview = Some(Preview {
            title,
            image: source,
        });
        self.image_preview_focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn close_image_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.image_preview.take().is_none() {
            return;
        }
        if let Some(focus) = self.image_preview_return_focus.take() {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn image_preview_layer(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let preview = self.image_preview.as_ref()?;
        let workspace = cx.weak_entity();
        Some(image_viewer::preview_layer(
            preview,
            &self.image_preview_focus,
            move |window, cx| {
                _ = workspace.update(cx, |this, cx| {
                    this.close_image_preview(window, cx);
                });
            },
        ))
    }
}

pub(super) fn decode_preview(
    path: &Path,
    bytes: &[u8],
    renderer: SvgRenderer,
    max_width: u32,
    max_height: u32,
) -> anyhow::Result<Arc<RenderImage>> {
    use gpui::ImageFormat as GpuiFormat;
    use image::ImageFormat::*;
    let format = match image_viewer::format(path) {
        Some(GpuiFormat::Svg) => {
            let svg = renderer.parse_standalone_svg(std::str::from_utf8(bytes)?)?;
            let intrinsic = svg.size();
            let scale = (max_width as f32 / intrinsic.width.as_f32())
                .min(max_height as f32 / intrinsic.height.as_f32())
                .min(1.);
            let dimension =
                |length: Pixels| DevicePixels((length.as_f32() * scale).round().max(1.) as i32);
            return Ok(renderer.render_parsed(
                &svg,
                SvgSize::ExactSize(size(
                    dimension(intrinsic.width),
                    dimension(intrinsic.height),
                )),
            )?);
        }
        Some(GpuiFormat::Png) => Png,
        Some(GpuiFormat::Jpeg) => Jpeg,
        Some(GpuiFormat::Gif) => Gif,
        Some(GpuiFormat::Webp) => WebP,
        Some(GpuiFormat::Bmp) => Bmp,
        Some(GpuiFormat::Tiff) => Tiff,
        Some(GpuiFormat::Ico) => Ico,
        Some(GpuiFormat::Pnm) => Pnm,
        None => anyhow::bail!("Unsupported image format"),
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    use image::ImageDecoder as _;
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = image::DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let mut frame = if image.width() > max_width || image.height() > max_height {
        image.thumbnail(max_width, max_height).into_rgba8()
    } else {
        image.into_rgba8()
    };
    for pixel in frame.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new([image::Frame::new(frame)])))
}

#[derive(Clone, Default)]
pub(super) struct ImageHoverDismiss(Rc<RefCell<Option<WeakEntity<ImageHover>>>>);

impl ImageHoverDismiss {
    pub(super) fn dismiss(&self, window: &mut Window, cx: &mut App) {
        let hover = self.0.borrow_mut().take();
        if let Some(hover) = hover {
            _ = hover.update(cx, |hover, cx| {
                hover.closed = true;
                hover.task = Task::ready(());
                if let Some(Ok(image)) = hover.image.take() {
                    _ = window.drop_image(image);
                }
                cx.notify();
            });
        }
    }
}

const HOVER_PADDING: Pixels = px(6.);

#[derive(Clone)]
pub(super) struct ImageHoverSource {
    read: Arc<dyn Fn() -> anyhow::Result<Vec<u8>> + Send + Sync>,
    path: Arc<Path>,
}

impl ImageHoverSource {
    pub(super) fn new(
        path: std::path::PathBuf,
        read: impl Fn() -> anyhow::Result<Vec<u8>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            read: Arc::new(read),
            path: path.into(),
        }
    }
}

pub(super) fn hover_image(
    trigger: Stateful<gpui::Div>,
    dismiss_on_diff_scroll: Option<ImageHoverDismiss>,
    read: impl Fn() -> anyhow::Result<Vec<u8>> + Send + Sync + 'static,
    path: std::path::PathBuf,
    info: Option<FileInfo>,
) -> Stateful<gpui::Div> {
    hover_image_with_info(
        trigger,
        dismiss_on_diff_scroll,
        ImageHoverSource::new(path, read),
        info,
    )
}

pub(super) fn hover_image_source(
    trigger: Stateful<gpui::Div>,
    dismiss_on_diff_scroll: Option<ImageHoverDismiss>,
    source: ImageHoverSource,
) -> Stateful<gpui::Div> {
    hover_image_with_info(trigger, dismiss_on_diff_scroll, source, None)
}

fn hover_image_with_info(
    trigger: Stateful<gpui::Div>,
    dismiss_on_diff_scroll: Option<ImageHoverDismiss>,
    ImageHoverSource { read, path }: ImageHoverSource,
    info: Option<FileInfo>,
) -> Stateful<gpui::Div> {
    const MAX_SIZE: Size<Pixels> = size(px(320.), px(240.));
    trigger.managed_tooltip_request(true, move |bounds, window, _| {
        // A moving row should dismiss the preview without starting another decode.
        if !window.is_pointer_motion() {
            return None;
        }
        let arrow = ArrowHover::new(bounds, Placement::Right, window);
        let dimensions = size(
            MAX_SIZE.width.min(arrow.width),
            MAX_SIZE.height.min(arrow.height),
        );
        if dimensions.width < px(32.) || dimensions.height < px(32.) {
            return None;
        }
        let read = read.clone();
        let path = path.clone();
        // The tooltip provider calls its builder again during animation and redraw.
        let view = OnceCell::new();
        let dismiss = dismiss_on_diff_scroll.clone();
        let info = info.clone();
        let request = TooltipRequest::new(bounds, move |window, cx| {
            view.get_or_init(|| {
                let read = read.clone();
                let path = path.clone();
                let scale = window.scale_factor();
                let renderer = cx.svg_renderer();
                let inset = (HOVER_PADDING + px(1.)) * 2.;
                let info = info.as_ref().map(|info| {
                    let card = info.build(None, cx);
                    card.update(cx, |card, _| card.embed(dimensions.width));
                    card
                });
                let hover = cx.new(|cx| {
                    cx.on_release(|hover: &mut ImageHover, cx| {
                        if let Some(Ok(image)) = hover.image.take() {
                            cx.defer(move |cx| cx.drop_image(image, None));
                        }
                    })
                    .detach();
                    let load = cx.background_spawn(async move {
                        decode_preview(
                            &path,
                            &read()?,
                            renderer,
                            ((dimensions.width - inset).as_f32() * scale).ceil() as u32,
                            ((dimensions.height - inset).as_f32() * scale).ceil() as u32,
                        )
                    });
                    let task = cx.spawn(async move |this, cx| {
                        let result = load.await;
                        _ = this.update(cx, |this: &mut ImageHover, cx| {
                            this.image = Some(result);
                            cx.notify();
                        });
                    });
                    ImageHover {
                        dimensions,
                        arrow,
                        info,
                        image: None,
                        task,
                        closed: false,
                    }
                });
                if let Some(dismiss) = &dismiss {
                    *dismiss.0.borrow_mut() = Some(hover.downgrade());
                }
                hover
            })
            .clone()
            .into()
        })
        .with_positioner(move || arrow.positioner())
        .paint_only();
        Some(request)
    })
}

struct ImageHover {
    dimensions: Size<Pixels>,
    arrow: ArrowHover,
    info: Option<gpui::Entity<Card>>,
    image: Option<anyhow::Result<Arc<RenderImage>>>,
    task: Task<()>,
    closed: bool,
}

impl Render for ImageHover {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.closed {
            return div().into_any_element();
        }
        let width = self.dimensions.width;
        let height = self.dimensions.height;
        let content = match &self.image {
            Some(Ok(image)) => {
                let pixels = image.size(0);
                let device_scale = window.scale_factor();
                img(image.clone())
                    .debug_selector(|| "image-hover-loaded".into())
                    .w(px(pixels.width.0 as f32 / device_scale))
                    .h(px(pixels.height.0 as f32 / device_scale))
                    .max_w_full()
                    .max_h_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element()
            }
            Some(Err(_)) => div().child(t!("image.open_failed")).into_any_element(),
            None => div().child(t!("image.loading")).into_any_element(),
        };
        let preview = div()
            .debug_selector(|| "image-hover-preview".into())
            .flex()
            .items_center()
            .justify_center()
            .w(width)
            .h(height)
            .min_h_0()
            .overflow_hidden()
            .p(HOVER_PADDING)
            .child(content);
        self.arrow
            .surface(window, cx)
            .debug_selector(|| "image-hover-surface".into())
            .relative()
            .w(width)
            .max_h(self.arrow.height)
            .children(self.info.clone())
            .child(preview)
            .into_any_element()
    }
}
