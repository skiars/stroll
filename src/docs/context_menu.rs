use gpui::{App, ClipboardItem, Pixels, Point, SharedString, WeakEntity, Window};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit_assets::IconName;
use rust_i18n::t;

use super::{Destination, DocReader, destination, is_markdown, online_images};

pub(super) fn show(
    reader: WeakEntity<DocReader>,
    link: Option<SharedString>,
    image: Option<SharedString>,
    position: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    window.defer(cx, move |window, cx| {
        _ = reader.update(cx, |doc, cx| {
            let menu = PopupMenu::build(window, cx, |mut menu, _, _| {
                if let Some(link) = link {
                    menu = doc.link_menu(menu, link);
                }
                if let Some(image) = image {
                    let items = doc.image_menu_items(image, &reader);
                    if !items.is_empty() && !menu.is_empty() {
                        menu = menu.separator();
                    }
                    for item in items {
                        menu = menu.item(item);
                    }
                }
                menu
            });
            if !menu.read(cx).is_empty() {
                PopupMenu::show_at(menu, position, window, cx);
            }
        });
    });
}

impl DocReader {
    pub(super) fn images(&self) -> online_images::DocumentImages {
        online_images::DocumentImages {
            root: self.root.clone(),
            document: self.path.clone(),
            remote: self.document.remote.clone(),
            allow_embedded: self.standalone,
            display: self.displayed_images.clone(),
        }
    }

    pub(super) fn preview_image(&self, uri: &SharedString, window: &mut Window, cx: &mut App) {
        let Some(source) = self.images().preview_source(uri, cx) else {
            return;
        };
        let title = if online_images::is_data_url(uri) {
            t!("image.embedded").into_owned().into()
        } else {
            uri.clone()
        };
        _ = self.workspace.update(cx, |workspace, cx| {
            workspace.open_image_preview(source, title, window, cx)
        });
    }

    fn link_menu(&self, mut menu: PopupMenu, link: SharedString) -> PopupMenu {
        let open = match destination(&self.path, &link) {
            Some(Destination::Local { path, fragment }) => {
                let label = if path == self.path && fragment.is_some() {
                    t!("docs.jump_to_section")
                } else if self.standalone && path != self.path {
                    t!("docs.open_in_window")
                } else if is_markdown(&path) {
                    t!("docs.open_in_reader")
                } else {
                    t!("docs.open_in_workspace")
                };
                Some(self.open_local_item(label, link.clone()))
            }
            Some(Destination::External(url)) => {
                let label = if url.scheme() == "mailto" {
                    t!("terminal.open_link")
                } else {
                    t!("docs.open_in_browser")
                };
                Some(PopupMenuItem::link(label, url.as_str()))
            }
            None => None,
        };
        if let Some(open) = open {
            menu = menu.item(open);
        }
        menu.item(copy_item(t!("terminal.copy_link"), link))
    }

    fn image_menu_items(
        &self,
        image: SharedString,
        reader: &WeakEntity<Self>,
    ) -> Vec<PopupMenuItem> {
        let mut items = Vec::new();
        if self.images().can_preview(&image) {
            let reader = reader.clone();
            let uri = image.clone();
            items.push(
                PopupMenuItem::new(t!("docs.preview_image"))
                    .icon(IconName::Eye)
                    .on_click(move |_, window, cx| {
                        let reader = reader.clone();
                        let uri = uri.clone();
                        // Let the menu restore focus before the preview saves it.
                        window.defer(cx, move |window, cx| {
                            _ = reader.update(cx, |doc, cx| doc.preview_image(&uri, window, cx));
                        });
                    }),
            );
        }
        if online_images::is_data_url(&image) {
            return items;
        }
        let label = match destination(&self.path, &image) {
            Some(Destination::Local { path, .. }) => {
                if !self.standalone && crate::image_viewer::is_image(&path) {
                    items.push(self.open_local_item(t!("docs.open_in_workspace"), image.clone()));
                }
                t!("docs.copy_image_path")
            }
            destination => {
                if let Some(Destination::External(url)) = destination
                    && matches!(url.scheme(), "http" | "https")
                {
                    items.push(PopupMenuItem::link(
                        t!("docs.open_in_browser"),
                        url.as_str(),
                    ));
                }
                t!("docs.copy_image_address")
            }
        };
        items.push(copy_item(label, image));
        items
    }

    fn open_local_item(&self, label: impl Into<SharedString>, link: SharedString) -> PopupMenuItem {
        let workspace = self.workspace.clone();
        let path = self.path.clone();
        PopupMenuItem::new(label).on_click(move |_, window, cx| {
            _ = workspace.update(cx, |workspace, cx| {
                workspace.follow_document_link(&path, &link, window, cx);
            });
        })
    }
}

fn copy_item(label: impl Into<SharedString>, target: SharedString) -> PopupMenuItem {
    PopupMenuItem::new(label)
        .icon(IconName::Copy)
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(target.to_string()));
        })
}
