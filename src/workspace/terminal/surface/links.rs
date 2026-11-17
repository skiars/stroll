use gpui::*;
use gpui_component::{
    menu::{PopupMenu, PopupMenuItem},
    tooltip::{ManagedTooltipExt as _, Tooltip, TooltipRequest},
};
use gpui_kit_assets::IconName;
use rust_i18n::t;

use crate::terminal::{Emulator, Link};

use super::Session;

pub(super) fn link_modifier(mods: Modifiers, cx: &App) -> bool {
    mods == Modifiers::secondary_key(cx)
}

impl Session {
    pub(super) fn reports_mouse(&self) -> bool {
        self.emulator.as_ref().is_some_and(Emulator::reports_mouse)
    }

    pub(super) fn link_at(&self, position: Point<Pixels>) -> Option<&Link> {
        let geometry = self.geometry;
        let edge = geometry.origin(usize::from(geometry.cols), usize::from(geometry.rows));
        if !geometry.bounds.contains(&position) || position.x >= edge.x || position.y >= edge.y {
            return None;
        }
        let (x, y) = geometry.cell_at(position);
        self.frame.link_at(x, y)
    }

    pub(super) fn set_link_hover(
        &mut self,
        position: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.link_hover.and_then(|point| self.link_at(point))
            != position.and_then(|point| self.link_at(point))
        {
            cx.notify();
        }
        self.link_hover = position;
    }

    pub(super) fn hovered_link(&self) -> Option<&Link> {
        if self.dragging_selection || self.reports_mouse() {
            return None;
        }
        self.link_at(self.link_hover?)
    }

    pub(super) fn link_tooltips(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        if self.geometry.rows == 0
            || self.frame.links.is_empty()
            || self.reports_mouse()
            || self.dragging_selection
        {
            return None;
        }
        let geometry = self.geometry;
        let cols = usize::from(geometry.cols);
        let rows = usize::from(geometry.rows);
        let gesture = if cx.platform_info().is_mac() {
            t!("terminal.link_click_command")
        } else {
            t!("terminal.link_click_control")
        };
        let pointing = link_modifier(window.modifiers(), cx);
        let mut overlay = div().size_full();
        for link in &self.frame.links {
            let url = SharedString::from(link.target.as_ref());
            let hint = SharedString::from(
                t!(
                    "terminal.link_hint",
                    url = url.as_ref(),
                    gesture = gesture.as_ref()
                )
                .into_owned(),
            );
            for y in link.rows(rows) {
                let Some(columns) = link.columns(y, cols) else {
                    continue;
                };
                let offset = geometry.origin(columns.start, y) - geometry.bounds.origin;
                let hint = hint.clone();
                let mut trigger = div()
                    .id(ElementId::named_usize(
                        url.clone(),
                        y * cols + columns.start,
                    ))
                    .absolute()
                    .left(offset.x)
                    .top(offset.y)
                    .w(geometry.cell.width * columns.len() as f32)
                    .h(geometry.cell.height)
                    .managed_tooltip_request(true, move |bounds, _, _| {
                        let hint = hint.clone();
                        Some(
                            TooltipRequest::new(bounds, move |window, cx| {
                                Tooltip::new(hint.clone()).build(window, cx)
                            })
                            .paint_only(),
                        )
                    });
                if pointing {
                    trigger = trigger.cursor_pointer();
                }
                overlay = overlay.child(trigger);
            }
        }
        let bounds = geometry.bounds;
        let mut overlay = overlay.into_any_element();
        overlay.layout_as_root(
            size(bounds.size.width.into(), bounds.size.height.into()),
            window,
            cx,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            overlay.prepaint_at(bounds.origin, window, cx);
        });
        Some(overlay)
    }

    pub(super) fn context_menu(menu: PopupMenu, session: &Entity<Self>, cx: &mut App) -> PopupMenu {
        let target = session.update(cx, |this, _| this.context_link.take());
        let terminal = session.read(cx);
        let copy = session.downgrade();
        let paste = copy.clone();
        let mut menu = menu
            .item(
                PopupMenuItem::new(t!("terminal.copy"))
                    .icon(IconName::Copy)
                    .disabled(terminal.selection_text().is_none())
                    .on_click(move |_, _, cx| {
                        _ = copy.update(cx, |this, cx| this.copy_selection(cx));
                    }),
            )
            .item(
                PopupMenuItem::new(t!("terminal.paste"))
                    .icon(IconName::ClipboardPaste)
                    .disabled(!terminal.can_paste())
                    .on_click(move |_, _, cx| {
                        _ = paste.update(cx, |this, cx| this.paste_clipboard(cx));
                    }),
            );
        if let Some(target) = target {
            menu = menu
                .separator()
                .item(PopupMenuItem::link(
                    t!("terminal.open_link"),
                    target.to_string(),
                ))
                .item(
                    PopupMenuItem::new(t!("terminal.copy_link"))
                        .icon(IconName::Link2)
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(target.to_string()));
                        }),
                );
        }
        menu
    }
}
