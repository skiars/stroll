//! Transient messages for the workspace.
//!
//! Every message is a bottom-right toast owned by the component library, so no
//! message takes part in the editor layout. Saving, a conflict, or a failed
//! operation must never move the text the reader is looking at.
//!
//! A toast reports *events*: something happened once and the reader may want to
//! know now. A *state* that lasts, such as a draft that conflicts with the file
//! on disk, is marked where it lives (the tab badge) and explained where the
//! reader acts on it (the save prompt), so it is never announced twice.

use gpui::{Anchor, App, SharedString, Window};
use gpui_component::{WindowExt as _, notification::Notification};

/// The corner every toast uses, and the default the theme advertises.
pub(super) const PLACEMENT: Anchor = Anchor::BottomRight;

/// Report a condition that needs attention but blocks nothing.
pub(super) fn info(window: &mut Window, cx: &mut App, message: impl Into<SharedString>) {
    push(Notification::info(message), window, cx);
}

/// Report an operation that failed.
pub(super) fn error(window: &mut Window, cx: &mut App, message: impl Into<SharedString>) {
    push(Notification::error(message).autohide(false), window, cx);
}

fn push(notification: Notification, window: &mut Window, cx: &mut App) {
    window.push_notification(notification.placement(PLACEMENT), cx);
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use gpui::{
        AppContext as _, Context, Render, TestAppContext, TestDispatcher, VisualTestContext,
        Window, div, px, size,
    };
    use gpui_component::{Root, notification::Notification};

    struct Probe;

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            div()
        }
    }

    /// Show one notification in a window of its own, then inspect the toast.
    fn with_toast(notification: Notification, check: impl FnOnce(&mut VisualTestContext)) {
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| Probe);
            Root::new(view, window, cx)
        });
        visual.simulate_resize(size(px(600.), px(400.)));
        visual.update(|window, cx| {
            window.push_notification(notification.placement(PLACEMENT), cx);
        });
        for _ in 0..6 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        check(visual);
    }

    /// The icon shares its top edge with the first line of the message.
    ///
    /// A fixed offset from the card padded the icon down the line box, which
    /// reads as a low icon next to a title.
    #[::core::prelude::v1::test]
    fn the_icon_sits_on_the_first_line_of_a_titled_toast() {
        with_toast(
            Notification::warning("The file changed on disk.").title("File changed on disk"),
            |visual| {
                let icon = visual.debug_bounds("notification-icon").expect("icon");
                let title = visual.debug_bounds("notification-title").expect("title");
                assert_eq!(icon.top(), title.top(), "the icon follows the title");
            },
        );
    }

    #[::core::prelude::v1::test]
    fn the_icon_sits_on_the_first_line_of_a_plain_toast() {
        with_toast(
            Notification::error("A message long enough to wrap onto a second line."),
            |visual| {
                let icon = visual.debug_bounds("notification-icon").expect("icon");
                let message = visual
                    .debug_bounds("notification-message")
                    .expect("message");
                assert_eq!(icon.top(), message.top(), "the icon follows the message");
            },
        );
    }
}
