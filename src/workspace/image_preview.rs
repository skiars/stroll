use gpui::{
    Anchor, AnyElement, App, AppContext, Context, ImageSource, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement, Pixels, Render, RenderImage, SharedString, Size,
    Stateful, Styled, StyledImage as _, SvgRenderer, Task, WeakEntity, Window, div, img, point, px,
    size,
};
use gpui_component::{
    Placement, ThemeStyled as _,
    tooltip::{ManagedTooltipExt as _, Positioner, TooltipRequest},
};
use rust_i18n::t;
use std::{
    cell::{OnceCell, RefCell},
    io::Cursor,
    path::Path,
    rc::Rc,
    sync::Arc,
};

use super::Workspace;
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
            return Ok(renderer
                .render_standalone_svg(std::str::from_utf8(bytes)?, max_width.max(max_height))?);
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

pub(super) fn hover_image(
    trigger: Stateful<gpui::Div>,
    dismiss_on_diff_scroll: Option<ImageHoverDismiss>,
    read: impl Fn() -> anyhow::Result<Vec<u8>> + Send + Sync + 'static,
    path: std::path::PathBuf,
) -> Stateful<gpui::Div> {
    const MAX_SIZE: Size<Pixels> = size(px(320.), px(240.));
    let read = Arc::new(read);
    let beside_tree = dismiss_on_diff_scroll.is_none();
    trigger.managed_tooltip_request(true, move |bounds, window, _| {
        // A moving row should dismiss the preview without starting another decode.
        if !window.is_pointer_motion() {
            return None;
        }
        let viewport = Positioner::viewport_bounds(window, px(8.));
        let gap = px(8.);
        let mut dimensions = size(
            MAX_SIZE.width.min(viewport.size.width),
            MAX_SIZE.height.min(viewport.size.height),
        );
        let positioner = if beside_tree {
            let right = viewport.right() - bounds.right() - gap;
            let left = bounds.left() - viewport.left() - gap;
            let (anchor, x, available) = if right >= px(160.) || right >= left {
                (Anchor::TopLeft, bounds.right() + gap, right)
            } else {
                (Anchor::TopRight, bounds.left() - gap, left)
            };
            dimensions.width = dimensions.width.min(available);
            Some((anchor, point(x, bounds.top())))
        } else {
            None
        };
        if dimensions.width < px(32.) || dimensions.height < px(32.) {
            return None;
        }
        let read = read.clone();
        let path = path.clone();
        // The tooltip provider calls its builder again during animation and redraw.
        let view = OnceCell::new();
        let dismiss = dismiss_on_diff_scroll.clone();
        let request = TooltipRequest::new(bounds, move |window, cx| {
            view.get_or_init(|| {
                let read = read.clone();
                let path = path.clone();
                let scale = window.scale_factor();
                let renderer = cx.svg_renderer();
                let inset = (HOVER_PADDING + px(1.)) * 2.;
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
        .with_placement(Placement::Right)
        .paint_only();
        Some(if let Some((anchor, position)) = positioner {
            request.with_positioner(move || Positioner::corner(anchor, position).margin(px(8.)))
        } else {
            request
        })
    })
}

struct ImageHover {
    dimensions: Size<Pixels>,
    image: Option<anyhow::Result<Arc<RenderImage>>>,
    task: Task<()>,
    closed: bool,
}

impl Render for ImageHover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.closed {
            return div().into_any_element();
        }
        let width = self.dimensions.width;
        let height = self.dimensions.height;
        let content = match &self.image {
            Some(Ok(image)) => img(image.clone())
                .debug_selector(|| "image-hover-loaded".into())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            Some(Err(_)) => div().child(t!("image.open_failed")).into_any_element(),
            None => div().child(t!("image.loading")).into_any_element(),
        };
        div()
            .debug_selector(|| "image-hover-preview".into())
            .flex()
            .items_center()
            .justify_center()
            .w(width)
            .h(height)
            .p(HOVER_PADDING)
            .popover_style(cx)
            .child(content)
            .into_any_element()
    }
}
