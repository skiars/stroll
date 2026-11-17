use gpui::{AnyElement, Context, ImageSource, SharedString, Window};

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
