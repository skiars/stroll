use std::path::Path;

use gpui::{
    AnyElement, App, FocusHandle, ImageFormat, ImageSource, InteractiveElement as _, IntoElement,
    ObjectFit, ParentElement as _, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Window, deferred, hsla, img, px, relative,
};
use gpui_component::{MODAL_POPUP_PRIORITY, PopupFrame};
use gpui_kit_assets::IconName;
use rust_i18n::t;

use crate::workspace::icon;

const PREVIEW_GROUP: &str = "image-preview";

#[derive(Clone)]
pub(crate) struct Preview {
    pub title: SharedString,
    pub image: ImageSource,
}

pub(crate) fn is_image(path: &Path) -> bool {
    format(path).is_some()
}

pub(crate) fn format(path: &Path) -> Option<ImageFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::Webp,
        "gif" => ImageFormat::Gif,
        "svg" => ImageFormat::Svg,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        "pnm" | "ppm" | "pgm" | "pbm" => ImageFormat::Pnm,
        _ => return None,
    })
}

pub(crate) fn preview_layer(
    preview: &Preview,
    focus: &FocusHandle,
    on_close: impl Fn(&mut Window, &mut App) + Clone + 'static,
) -> AnyElement {
    let close_backdrop = on_close.clone();
    let close_key = on_close.clone();
    let close_button = on_close;

    let content = img(preview.image.clone())
        .object_fit(ObjectFit::Contain)
        .max_w(relative(0.86))
        .max_h(relative(0.86))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        });

    let mut backdrop = gpui::div()
        .id("image-preview")
        .role(Role::Dialog)
        .aria_label(preview.title.clone())
        .absolute()
        .inset_0()
        .occlude()
        .group(PREVIEW_GROUP)
        .bg(hsla(0., 0., 0., 0.44))
        .track_focus(focus)
        .focusable()
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape" {
                window.prevent_default();
                cx.stop_propagation();
                close_key(window, cx);
            }
        })
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            close_backdrop(window, cx);
        })
        .child(
            gpui::div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(content),
        )
        .child(
            gpui::div()
                .id("image-preview-close")
                .debug_selector(|| "image-preview-close".into())
                .role(Role::Button)
                .aria_label(t!("common.close"))
                .tab_index(0)
                .absolute()
                .top(px(48.))
                .right(px(14.))
                .size(px(32.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.32))
                .opacity(0.)
                .group_hover(PREVIEW_GROUP, |style| style.opacity(1.))
                .hover(|style| style.bg(hsla(0., 0., 0., 0.52)))
                .cursor_pointer()
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(move |_, window, cx| close_button(window, cx))
                .child(icon(IconName::X, hsla(0., 0., 1., 1.), 16.)),
        );
    backdrop.style().backdrop_blur = Some(px(24.));
    backdrop.style().backdrop_saturation = Some(1.15);

    deferred(PopupFrame::new(backdrop, false))
        .with_priority(MODAL_POPUP_PRIORITY + 1)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_extensions_are_case_insensitive() {
        for path in ["a.png", "a.JPG", "a.jpeg", "a.webp", "a.gif", "a.svg"] {
            assert!(is_image(Path::new(path)), "{path}");
        }
        for path in ["a.rs", "a.md", "a", "a.png.txt"] {
            assert!(!is_image(Path::new(path)), "{path}");
        }
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn preview_closes_with_escape_and_backdrop_but_not_on_the_image(cx: &mut gpui::TestAppContext) {
        use std::{
            cell::{Cell, RefCell},
            rc::Rc,
        };

        use gpui::{Context, Render, RenderImage, VisualTestContext, Window, point};
        use image::{Frame, Rgba, RgbaImage};

        struct PreviewRoot {
            source: ImageSource,
            focus: FocusHandle,
            closes: Rc<Cell<usize>>,
        }

        impl Render for PreviewRoot {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let closes = self.closes.clone();
                preview_layer(
                    &Preview {
                        title: "preview".into(),
                        image: self.source.clone(),
                    },
                    &self.focus,
                    move |_, _| closes.set(closes.get() + 1),
                )
            }
        }

        cx.update(gpui_component::init);
        let closes = Rc::new(Cell::new(0));
        let root_closes = closes.clone();
        let focus = Rc::new(RefCell::new(None));
        let focus_handle = focus.clone();
        let source = ImageSource::Render(std::sync::Arc::new(RenderImage::new([Frame::new(
            RgbaImage::from_pixel(20, 20, Rgba([80, 140, 220, 255])),
        )])));
        let (_, cx) = cx.add_window_view(move |_, cx| {
            let handle = cx.focus_handle();
            *focus_handle.borrow_mut() = Some(handle.clone());
            PreviewRoot {
                source,
                focus: handle,
                closes: root_closes.clone(),
            }
        });
        let cx: &mut VisualTestContext = cx;
        let focus = focus.borrow().clone().unwrap();
        cx.update(|window, cx| {
            focus.focus(window, cx);
            let _ = window.draw(cx);
        });

        cx.simulate_keystrokes("escape");
        assert_eq!(closes.get(), 1);

        cx.simulate_click(point(px(4.), px(4.)), Default::default());
        assert_eq!(closes.get(), 2);

        let center = cx.update(|window, _| window.viewport_size() / 2.);
        cx.simulate_click(point(center.width, center.height), Default::default());
        assert_eq!(closes.get(), 2, "clicking the image should keep it open");

        let viewport = cx.update(|window, _| window.viewport_size());
        cx.simulate_click(point(viewport.width - px(30.), px(64.)), Default::default());
        assert_eq!(closes.get(), 3, "the close button should close the preview");
    }
}
