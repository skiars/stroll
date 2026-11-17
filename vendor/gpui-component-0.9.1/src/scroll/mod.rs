mod scrollable;

pub use gpui_base::AutoScroll;
pub use gpui_base::ScrollableMask;
pub use gpui_base::{
    Scrollbar, ScrollbarAxis, ScrollbarEntrance, ScrollbarHandle, ScrollbarMode, ScrollbarMotion,
    ScrollbarStyles, ScrollbarThumbStyle, ScrollbarTrackStyle,
};
pub use scrollable::*;

/// Creates an overlay scrollbar with the narrow geometry used across the UI.
///
/// The 10 px track leaves room for a 4 px thumb inset by 3 px on each side,
/// matching the LSP hover popover. The theme still owns the thumb colour,
/// radius and hover expansion.
#[track_caller]
pub fn thin_scrollbar<H: ScrollbarHandle + Clone>(handle: &H) -> Scrollbar {
    Scrollbar::vertical(handle).styles(|styles| {
        styles
            .track(|track| track.width(gpui::px(10.)))
            .thumb(|thumb| thumb.width(gpui::px(4.)).inset(gpui::px(3.)))
    })
}

/// Creates a content scrollbar that stays visible after the idle hold.
///
/// The active theme's standard geometry (16 px track, 6 px thumb) is kept; only
/// the auto-hide policy changes.
#[track_caller]
pub fn content_scrollbar<H: ScrollbarHandle + Clone>(handle: &H) -> Scrollbar {
    Scrollbar::vertical(handle).mode(ScrollbarMode::Always)
}

/// Creates an overlay scrollbar for a popup body.
///
/// Popups use the same narrow geometry as every other non-content surface.
#[track_caller]
pub fn popup_scrollbar<H: ScrollbarHandle + Clone>(handle: &H) -> Scrollbar {
    thin_scrollbar(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts_base(_: gpui_base::Scrollbar) {}

    #[test]
    fn ui_scrollbar_is_the_base_type() {
        fn accepts_handle(_: impl gpui_base::ScrollbarHandle) {}

        let handle = gpui::ScrollHandle::default();
        let scrollbar: crate::scroll::Scrollbar = Scrollbar::vertical(&handle).styles(|styles| {
            styles
                .track(|style| style.bg(gpui::transparent_black()))
                .thumb(|style| style.bg(gpui::transparent_black()))
                .thumb_hover(|style| style.bg(gpui::transparent_black()))
                .thumb_active(|style| style.bg(gpui::transparent_black()))
        });
        accepts_base(scrollbar);
        accepts_handle(handle);
    }

    #[test]
    fn presets_build_base_scrollbars() {
        let handle = gpui::ScrollHandle::default();
        accepts_base(thin_scrollbar(&handle));
        accepts_base(popup_scrollbar(&handle));
        accepts_base(content_scrollbar(&handle));
        accepts_base(content_scrollbar(&handle).axis(ScrollbarAxis::Horizontal));
    }
}
