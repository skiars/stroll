use std::{borrow::Cow, cell::Cell, time::Instant};

use super::*;
use crate::image_viewer;

const ZOOM_STOPS: [f32; 12] = [0.25, 0.5, 0.75, 1., 1.25, 1.5, 1.75, 2., 2.5, 3., 3.5, 4.];
const SCROLL_STEP: Pixels = px(48.);
const SCROLL_IDLE: Duration = Duration::from_millis(250);
const FIT_MARGIN: Pixels = px(16.);

pub(super) struct ImageTab {
    loaded: Option<Result<ImageData, SharedString>>,
    task: Task<()>,
    viewport: Rc<Cell<Bounds<Pixels>>>,
    zoom: Option<f32>,
    zoom_scroll: ZoomScroll,
    sampling: ImageSampling,
    offset: Point<Pixels>,
    drag: Option<Point<Pixels>>,
}

#[derive(Default)]
struct ZoomScroll {
    remainder: f32,
    last_event: Option<Instant>,
}

impl ZoomScroll {
    fn push(&mut self, event: &ScrollEvent, now: Instant) -> i32 {
        if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            *self = Self::default();
            return 0;
        }
        if event.touch_phase == TouchPhase::Started
            || self
                .last_event
                .is_some_and(|last| now.duration_since(last) >= SCROLL_IDLE)
        {
            self.remainder = 0.;
        }
        let delta = match event.delta {
            // Some platforms scale wheel notches by the system's lines-per-scroll setting.
            ScrollDelta::Lines(delta) => delta.y.clamp(-1., 1.),
            ScrollDelta::Pixels(delta) => delta.y / SCROLL_STEP,
        };
        if delta == 0. || !delta.is_finite() {
            return 0;
        }
        self.last_event = Some(now);
        let total = self.remainder + delta;
        self.remainder = total.fract();
        total as i32
    }
}

fn stepped_zoom(current: f32, steps: i32) -> Option<f32> {
    if steps > 0 {
        let first = ZOOM_STOPS.partition_point(|&stop| stop <= current);
        (first < ZOOM_STOPS.len())
            .then(|| ZOOM_STOPS[(first + steps as usize - 1).min(ZOOM_STOPS.len() - 1)])
    } else if steps < 0 {
        let end = ZOOM_STOPS.partition_point(|&stop| stop < current);
        (end > 0).then(|| ZOOM_STOPS[end.saturating_sub(steps.unsigned_abs() as usize)])
    } else {
        None
    }
}

struct ImageData {
    image: Arc<RenderImage>,
    transparent: bool,
}

impl ImageTab {
    fn new() -> Self {
        Self {
            loaded: None,
            task: Task::ready(()),
            viewport: Rc::default(),
            zoom: None,
            zoom_scroll: ZoomScroll::default(),
            sampling: ImageSampling::default(),
            offset: Point::default(),
            drag: None,
        }
    }

    #[cfg(test)]
    pub(super) fn sampling(&self) -> ImageSampling {
        self.sampling
    }

    #[cfg(test)]
    pub(super) fn zoom(&self) -> Option<f32> {
        self.zoom
    }

    pub(super) fn release(&mut self, window: &mut Window) {
        if let Some(Ok(data)) = self.loaded.take() {
            _ = window.drop_image(data.image);
        }
    }

    fn fitted_zoom(viewport: Bounds<Pixels>, original: Size<Pixels>) -> f32 {
        let available = viewport.size - size(FIT_MARGIN * 2., FIT_MARGIN * 2.);
        (available.width.max(px(1.)) / original.width)
            .min(available.height.max(px(1.)) / original.height)
            .min(1.)
    }

    fn zoom_at(&mut self, steps: i32, anchor: Option<Point<Pixels>>, window: &Window) -> bool {
        let Some(Ok(data)) = &self.loaded else {
            return false;
        };
        let viewport = self.viewport.get();
        let original = display_size(&data.image, window);
        let fit = Self::fitted_zoom(viewport, original);
        let old = self.zoom.unwrap_or(fit);
        let Some(new) = stepped_zoom(old, steps) else {
            return false;
        };
        let anchor = anchor.unwrap_or_else(|| viewport.center()) - viewport.center();
        let offset = clamp_offset(self.offset, viewport.size, original.map(|side| side * old));
        self.offset = clamp_offset(
            anchor + (offset - anchor) * (new / old),
            viewport.size,
            original.map(|side| side * new),
        );
        self.zoom = Some(new);
        true
    }

    fn pan_to(&mut self, position: Point<Pixels>, window: &Window) {
        let (Some(previous), Some(zoom), Some(Ok(data))) = (self.drag, self.zoom, &self.loaded)
        else {
            return;
        };
        let viewport = self.viewport.get().size;
        let image = display_size(&data.image, window).map(|side| side * zoom);
        self.offset = clamp_offset(
            clamp_offset(self.offset, viewport, image) + position - previous,
            viewport,
            image,
        );
        self.drag = Some(position);
    }
}

fn display_size(image: &RenderImage, window: &Window) -> Size<Pixels> {
    image
        .size(0)
        .map(|value| px(value.0 as f32 / window.scale_factor()))
}

fn clamp_offset(
    offset: Point<Pixels>,
    viewport: Size<Pixels>,
    image: Size<Pixels>,
) -> Point<Pixels> {
    let horizontal = ((image.width - viewport.width) / 2.).max(px(0.));
    let vertical = ((image.height - viewport.height) / 2.).max(px(0.));
    point(
        offset.x.clamp(-horizontal, horizontal),
        offset.y.clamp(-vertical, vertical),
    )
}

impl Workspace {
    pub(super) fn load_image_tab(
        &mut self,
        path: PathBuf,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(repo), Some(format)) = (self.repository.clone(), image_viewer::format(&path))
        else {
            return;
        };
        let source = path.clone();
        let renderer = cx.svg_renderer();
        let load = cx.background_spawn(async move {
            let image =
                Image::from_bytes(format, image_bytes(&repo, &source)?).to_image_data(renderer)?;
            let dimensions = image.size(0);
            anyhow::ensure!(
                dimensions.width.0 > 0 && dimensions.height.0 > 0,
                t!("image.open_failed")
            );
            let transparent = (0..image.frame_count())
                .filter_map(|index| image.as_bytes(index))
                .any(|bytes| bytes.chunks_exact(4).any(|pixel| pixel[3] < 255));
            Ok::<_, anyhow::Error>(ImageData { image, transparent })
        });
        let key = path.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            _ = this.update_in(cx, |this, window, cx| {
                if let Some(tab) = this.images.get_mut(&key) {
                    tab.release(window);
                    tab.loaded = Some(result.map_err(|error| format!("{error:#}").into()));
                    cx.notify();
                }
            });
        });
        self.images.entry(path).or_insert_with(ImageTab::new).task = task;
    }

    pub(super) fn active_image_tab(&self) -> Option<(&Path, &ImageTab)> {
        let path = self.tabs.active()?;
        Some((path, self.images.get(path)?))
    }

    pub(super) fn remove_image_tab(&mut self, path: &Path, window: &mut Window) {
        if let Some(mut tab) = self.images.remove(path) {
            tab.release(window);
        }
    }

    pub(super) fn image_tab_content(
        &self,
        path: &Path,
        tab: &ImageTab,
        cx: &Context<Self>,
        colors: Palette,
    ) -> AnyElement {
        let pane = v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(colors.bg);
        let data = match &tab.loaded {
            Some(Ok(data)) => data,
            status => {
                let (label, detail) = match status {
                    Some(Err(detail)) => (t!("image.open_failed"), Some(detail.clone())),
                    _ => (t!("image.loading"), None),
                };
                return pane
                    .child(
                        v_flex()
                            .id("workspace-image-status")
                            .debug_selector(|| "workspace-image-status".into())
                            .flex_1()
                            .justify_center()
                            .items_center()
                            .gap_2()
                            .px_4()
                            .text_color(colors.muted)
                            .child(label)
                            .children(detail.map(|detail| div().text_sm().child(detail))),
                    )
                    .into_any_element();
            }
        };
        let dimensions = data.image.size(0);
        let zoom_label = tab.zoom.map(|zoom| format!("{}%", zoom * 100.));
        let controls = h_flex()
            .id("workspace-image-controls")
            .debug_selector(|| "workspace-image-controls".into())
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(FILE_TOOLBAR_HEIGHT)
            .px_2()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(colors.muted)
                    .child(format!("{} × {}", dimensions.width.0, dimensions.height.0)),
            )
            .child(
                icon_button("image-zoom-out", IconName::Minus, t!("image.zoom_out")).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.update_image_view(cx, |tab| {
                            tab.zoom_scroll = ZoomScroll::default();
                            tab.zoom_at(-1, None, window);
                        });
                    }),
                ),
            )
            .children(zoom_label.map(|label| div().text_sm().text_color(colors.muted).child(label)))
            .child(
                icon_button("image-zoom-in", IconName::Plus, t!("image.zoom_in")).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.update_image_view(cx, |tab| {
                            tab.zoom_scroll = ZoomScroll::default();
                            tab.zoom_at(1, None, window);
                        });
                    }),
                ),
            )
            .child(
                Button::new("image-actual-size")
                    .small()
                    .ghost()
                    .label("100%")
                    .tooltip(t!("image.actual_size"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_image_view(cx, |tab| {
                            tab.zoom_scroll = ZoomScroll::default();
                            tab.zoom = Some(1.);
                            tab.offset = Point::default();
                        });
                    })),
            )
            .child(
                Button::new("image-fit")
                    .small()
                    .ghost()
                    .label(t!("image.fit"))
                    .selected(tab.zoom.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_image_view(cx, |tab| {
                            tab.zoom_scroll = ZoomScroll::default();
                            tab.zoom = None;
                            tab.offset = Point::default();
                        });
                    })),
            );
        let controls = controls.child(self.image_sampling_control(path, tab.sampling, cx));
        let image = data.image.clone();
        let sampling = tab.sampling;
        let image_id = SharedString::from(paths::display(path));
        let transparent = data.transparent;
        let viewport = tab.viewport.clone();
        let zoom = tab.zoom;
        let offset = tab.offset;
        let image_element = canvas(
            move |bounds, window, cx| {
                viewport.set(bounds);
                let original = display_size(&image, window);
                let zoom = zoom.unwrap_or_else(|| ImageTab::fitted_zoom(bounds, original));
                let display = original.map(|side| side * zoom);
                let offset = clamp_offset(offset, bounds.size, display);
                let origin =
                    bounds.center() + offset - point(display.width / 2., display.height / 2.);
                let mut image = img(image)
                    .id(image_id)
                    .size_full()
                    .object_fit(ObjectFit::Fill)
                    .image_sampling(sampling)
                    .into_any_element();
                image.prepaint_as_root(origin, display.map(AvailableSpace::Definite), window, cx);
                (Bounds::new(origin, display), image)
            },
            move |viewport, (bounds, mut image), window, cx| {
                if transparent {
                    paint_checkerboard(viewport.intersect(&bounds), bounds.origin, window, colors);
                }
                image.paint(window, cx);
            },
        )
        .size_full();
        pane.child(
            div()
                .id("workspace-image-viewport")
                .debug_selector(|| "workspace-image-viewport".into())
                .aria_label(file_name(path).to_owned())
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .when(tab.drag.is_some(), |view| {
                    view.cursor(CursorStyle::ClosedHand)
                })
                .on_scroll(cx.listener(|this, event: &ScrollEvent, window, cx| {
                    if let Some(tab) = this
                        .tabs
                        .active()
                        .and_then(|path| this.images.get_mut(path))
                    {
                        if event.modifiers.control || event.modifiers.platform {
                            let steps = tab.zoom_scroll.push(event, cx.background_executor().now());
                            if steps != 0 && tab.zoom_at(steps, Some(event.position), window) {
                                cx.notify();
                            }
                            cx.stop_propagation();
                        } else {
                            tab.zoom_scroll = ZoomScroll::default();
                        }
                    }
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.update_image_view(cx, |tab| {
                            if let Some(zoom) = tab.zoom
                                && let Some(Ok(data)) = &tab.loaded
                            {
                                let display =
                                    display_size(&data.image, window).map(|side| side * zoom);
                                let viewport = tab.viewport.get().size;
                                if display.width > viewport.width
                                    || display.height > viewport.height
                                {
                                    tab.drag = Some(event.position);
                                }
                            }
                        });
                    }),
                )
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                    if event.pressed_button == Some(MouseButton::Left)
                        && this
                            .active_image_tab()
                            .is_some_and(|(_, tab)| tab.drag.is_some())
                    {
                        this.update_image_view(cx, |tab| tab.pan_to(event.position, window));
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.update_image_view(cx, |tab| tab.drag = None);
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        if this
                            .active_image_tab()
                            .is_some_and(|(_, tab)| tab.drag.is_some())
                        {
                            this.update_image_view(cx, |tab| tab.drag = None);
                        }
                    }),
                )
                .child(image_element),
        )
        .child(controls)
        .into_any_element()
    }

    fn image_sampling_control(
        &self,
        path: &Path,
        current: ImageSampling,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let workspace = cx.weak_entity();
        let path = path.to_owned();
        div()
            .id("workspace-image-sampling")
            .debug_selector(|| "workspace-image-sampling".into())
            .flex_shrink_0()
            .child(
                Button::new("image-sampling")
                    .ghost()
                    .label(sampling_label(current))
                    .accessibility_label(t!("image.sampling_mode", mode = sampling_label(current)))
                    .dropdown_caret(true)
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                        for sampling in [ImageSampling::Linear, ImageSampling::Nearest] {
                            let workspace = workspace.clone();
                            let path = path.clone();
                            menu = menu.item(
                                PopupMenuItem::new(sampling_label(sampling))
                                    .checked(sampling == current)
                                    .on_click(move |_, _, cx| {
                                        _ = workspace.update(cx, |this, cx| {
                                            if let Some(tab) = this.images.get_mut(&path) {
                                                tab.sampling = sampling;
                                                cx.notify();
                                            }
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
    }

    fn update_image_view(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut ImageTab)) {
        if let Some(tab) = self
            .tabs
            .active()
            .and_then(|path| self.images.get_mut(path))
        {
            update(tab);
            cx.notify();
        }
    }
}

fn sampling_label(sampling: ImageSampling) -> Cow<'static, str> {
    match sampling {
        ImageSampling::Linear => t!("image.sampling_linear"),
        ImageSampling::Nearest => t!("image.sampling_nearest"),
    }
}

fn image_bytes(repo: &Repository, path: &Path) -> Result<Vec<u8>> {
    if path.is_absolute() {
        let target = path.canonicalize()?;
        anyhow::ensure!(
            target.is_file() && target.components().all(|part| part.as_os_str() != ".git"),
            t!("storage.local_regular_file")
        );
        return Ok(fs::read(target)?);
    }
    if repo.ssh.is_some() {
        repo.bytes(path, true)
    } else {
        Ok(fs::read(repo.resolve(path)?)?)
    }
}

fn paint_checkerboard(
    visible: Bounds<Pixels>,
    origin: Point<Pixels>,
    window: &mut Window,
    colors: Palette,
) {
    let side = px(12.);
    let column = ((visible.left() - origin.x) / side).floor() as i32;
    let mut row = ((visible.top() - origin.y) / side).floor() as i32;
    let mut y = origin.y + side * row as f32;
    window.paint_quad(fill(visible, colors.soft));
    while y < visible.bottom() {
        let mut x = origin.x + side * (column + (row + column).rem_euclid(2)) as f32;
        while x < visible.right() {
            let square = Bounds::new(point(x, y), size(side, side));
            window.paint_quad(fill(square.intersect(&visible), colors.hover));
            x += side * 2.;
        }
        y += side;
        row += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn fixed_zoom_steps_start_from_fit_and_clamp_at_both_ends() {
        assert_eq!(stepped_zoom(0.62, 1), Some(0.75));
        assert_eq!(stepped_zoom(0.62, -1), Some(0.5));
        assert_eq!(stepped_zoom(0.04, 1), Some(0.25));
        assert_eq!(stepped_zoom(0.04, -1), None);
        assert_eq!(stepped_zoom(1., 3), Some(1.75));
        assert_eq!(stepped_zoom(1., -3), Some(0.25));
        assert_eq!(stepped_zoom(1., i32::MAX), Some(4.));
        assert_eq!(stepped_zoom(1., i32::MIN), Some(0.25));
        assert_eq!(stepped_zoom(4., 1), None);
        assert_eq!(stepped_zoom(0.25, -1), None);
    }

    #[::core::prelude::v1::test]
    fn precise_scroll_accumulates_and_resets_at_gesture_boundaries() {
        let mut scroll = ZoomScroll::default();
        let now = Instant::now();
        let mut event = ScrollEvent {
            delta: ScrollDelta::Pixels(point(px(0.), px(12.))),
            source: ScrollSource::Touchpad,
            touch_phase: TouchPhase::Started,
            ..Default::default()
        };
        assert_eq!(scroll.push(&event, now), 0);
        event.touch_phase = TouchPhase::Moved;
        assert_eq!(scroll.push(&event, now), 0);
        assert_eq!(scroll.push(&event, now), 0);
        assert_eq!(scroll.push(&event, now), 1);
        assert_eq!(scroll.push(&event, now), 0);
        event.touch_phase = TouchPhase::Cancelled;
        assert_eq!(scroll.push(&event, now), 0);
        event.touch_phase = TouchPhase::Moved;
        event.delta = ScrollDelta::Pixels(point(px(0.), px(36.)));
        assert_eq!(scroll.push(&event, now), 0);
    }

    #[::core::prelude::v1::test]
    fn wheels_support_whole_notches_and_fractional_scroll_bursts() {
        let mut scroll = ZoomScroll::default();
        let now = Instant::now();
        let mut event = ScrollEvent {
            delta: ScrollDelta::Lines(point(0., 1.)),
            ..Default::default()
        };
        assert_eq!(scroll.push(&event, now), 1);
        event.delta = ScrollDelta::Lines(point(0., 3.));
        assert_eq!(scroll.push(&event, now), 1);
        event.delta = ScrollDelta::Lines(point(0., -0.5));
        assert_eq!(scroll.push(&event, now), 0);
        assert_eq!(scroll.push(&event, now + SCROLL_IDLE), 0);
        assert_eq!(scroll.push(&event, now + SCROLL_IDLE), -1);
        event.delta = ScrollDelta::Pixels(point(px(0.), px(24.)));
        assert_eq!(scroll.push(&event, now + SCROLL_IDLE), 0);
        assert_eq!(scroll.push(&event, now + SCROLL_IDLE), 1);
    }
}
