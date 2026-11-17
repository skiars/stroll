use rust_i18n::t;

use std::path::Path;

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ElementExt as _, Icon, InteractiveElementExt as _, Placement,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt, PopupMenuItem},
    popover::{Popover, PopoverState},
    scroll::{ScrollableMask, Scrollbar, ScrollbarMode, popup_scrollbar},
    tooltip::{ManagedTooltipExt as _, Tooltip},
    v_flex,
};
use gpui_kit_assets::IconName;

use crate::{docs, paths, tabs::labels};

use super::{FILE_TOOLBAR_HEIGHT, Palette, Workspace, file_type_icon, icon_button};

impl Workspace {
    pub(super) fn tab_bar(&self, cx: &Context<Self>, colors: Palette) -> impl IntoElement {
        let labels = labels(self.tabs.paths());
        let scroll = self.tab_scroll.clone();
        let measure = cx.weak_entity();
        let width = self.tab_viewport_width;
        let reveal = self.reveal_tab;
        let active = self.tabs.active().and_then(|path| self.tabs.index(path));
        let was_overflow = self.tabs_overflow;
        let more = was_overflow || self.open_files_list;
        let strip = h_flex()
            .id("editor-tab-strip")
            .role(Role::TabList)
            .aria_label(t!("tabs.open_files"))
            .h(px(26.))
            .w_full()
            .min_w_0()
            .gap(px(6.))
            .overflow_x_scroll()
            .lock_scroll_axis()
            .track_scroll(&scroll)
            .on_prepaint(move |bounds, window, _| {
                // The trigger uses 28px plus the toolbar gap. Measure against
                // the space available without it so it can disappear again.
                let full_width = bounds.size.width + if more { px(34.) } else { px(0.) };
                let content_width = bounds.size.width + scroll.max_offset().x;
                let overflow = content_width > full_width + px(1.);
                let resized = bounds.size.width != width;
                let reveal_offset = if reveal || resized {
                    active
                        .and_then(|index| scroll.bounds_for_item(index))
                        .map(|item| {
                            let offset = scroll.offset().x;
                            if item.left() + offset < bounds.left() {
                                bounds.left() - item.left()
                            } else if item.right() + offset > bounds.right() {
                                bounds.right() - item.right()
                            } else {
                                offset
                            }
                        })
                } else {
                    None
                };
                if resized || reveal || overflow != was_overflow {
                    window.on_next_frame(move |_, cx| {
                        _ = measure.update(cx, |this, cx| {
                            this.tab_viewport_width = bounds.size.width;
                            this.reveal_tab = false;
                            this.tabs_overflow = overflow;
                            if let Some(offset) = reveal_offset {
                                this.tab_scroll.set_offset(point(offset, px(0.)));
                            }
                            cx.notify();
                        });
                    });
                }
            })
            .children(
                self.tabs
                    .paths()
                    .iter()
                    .zip(&labels)
                    .map(|(path, label)| self.tab_element(path, label, cx, colors)),
            );

        h_flex()
            .id("editor-tab-bar")
            .debug_selector(|| "workspace-tab-bar".into())
            .h(FILE_TOOLBAR_HEIGHT)
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .gap(px(6.))
            .px_2()
            .bg(colors.bg)
            .children(self.show_file_tree_button(cx))
            .child(
                // Fill the bar so the overflow indicator can sit in the gutter
                // below the 26 px tabs instead of crossing their text.
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(strip)
                    .child(ScrollableMask::vertical_wheel(&self.tab_scroll))
                    .when(self.tabs_overflow, |this| {
                        this.child(tab_overflow_indicator(&self.tab_scroll, colors))
                    }),
            )
            .when(more, |bar| {
                bar.child(self.open_files_popover(labels, cx, colors))
            })
            .child(self.workspace_actions(cx))
    }

    fn tab_element(
        &self,
        path: &Path,
        label: &str,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let active = self.tabs.active() == Some(path);
        let select = path.to_owned();
        let debug_select = format!("select-tab:{}", paths::display(path));
        let tooltip = if path.is_absolute() {
            format!("{} · {}", t!("tabs.local_file"), paths::display(path))
        } else {
            paths::display(path)
        };
        let tab_group = SharedString::from(format!("editor-tab:{tooltip}"));
        let tab = h_flex()
            .id(SharedString::from(tooltip.clone()))
            .group(tab_group.clone())
            .h(px(26.))
            .min_w(px(108.))
            .max_w(px(240.))
            .flex_shrink_0()
            .pl(px(6.))
            .pr(px(2.))
            .gap(px(2.))
            .rounded(px(8.))
            .border_1()
            .border_color(transparent_black())
            .text_color(colors.muted)
            .when(active, |tab| {
                tab.bg(colors.elevated)
                    .border_color(colors.surface)
                    .text_color(colors.ink)
            })
            .when(!active, |tab| {
                tab.bg(colors.soft)
                    .hover(|style| style.bg(colors.hover).text_color(colors.ink))
            })
            .child(
                h_flex()
                    .id("select-tab")
                    .debug_selector(move || debug_select)
                    .role(Role::Tab)
                    .aria_selected(active)
                    .aria_label(tooltip.clone())
                    .tab_index(0)
                    .tab_stop(active)
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .managed_tooltip_restarting_at(Placement::Bottom, move |_, cx| {
                        cx.new(|_| Tooltip::new(tooltip.clone())).into()
                    })
                    .on_mouse_down(MouseButton::Left, {
                        let select = select.clone();
                        cx.listener(move |this, _, window, cx| {
                            this.activate_tab(&select, window, cx);
                        })
                    })
                    // Only a double click pins the preview tab; activation already
                    // happened on mouse down.
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        if event.click_count() > 1 {
                            this.keep_tab(&select, cx);
                        }
                    }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .mr_1()
                            .child(file_type_icon(path, colors, 12.)),
                    )
                    .child(
                        div()
                            .truncate()
                            .min_w_0()
                            .when(self.tabs.is_preview(path), Styled::italic)
                            .child(label.to_owned()),
                    )
                    .when(
                        self.tab_is_dirty(path) || self.tab_needs_attention(path),
                        |tab| tab.child(dirty_mark(colors, self.tab_needs_attention(path))),
                    ),
            )
            .child(tab_close_button(
                path,
                cx.weak_entity(),
                active,
                colors,
                tab_group,
            ));
        self.tab_context_menu(tab, path, cx)
    }

    fn open_files_popover(
        &self,
        labels: Vec<String>,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let workspace = cx.weak_entity();
        let scroll = self.open_files_scroll.clone();
        Popover::new("all-open-files")
            .dropdown_motion()
            .scrollable(false)
            .anchor(Anchor::TopRight)
            .offset(px(6.))
            .p_2()
            .open(self.open_files_list)
            .on_open_change(cx.listener(move |this, &open, _, cx| {
                this.open_files_list = open;
                if open
                    && let Some(index) = this.tabs.active().and_then(|path| this.tabs.index(path))
                {
                    this.open_files_scroll
                        .scroll_to_item(index, ScrollStrategy::Center);
                }
                cx.notify();
            }))
            .trigger(
                icon_button(
                    "open-files-trigger",
                    IconName::Maximize2,
                    t!("tabs.open_files"),
                )
                .tooltip_placement(Placement::Bottom),
            )
            .content(move |_, window, cx| {
                let popup = cx.weak_entity();
                let width = px(440.).min(window.viewport_size().width - px(32.));
                let rows = workspace
                    .read_with(cx, |this, _| this.tabs.paths().len())
                    .unwrap_or_default();
                let height = (px(46.) * rows as f32)
                    .min(px(400.))
                    .min(window.viewport_size().height * 0.55);
                let workspace = workspace.clone();
                let labels = labels.clone();
                let list = uniform_list("open-files-list", rows, move |range, _, cx| {
                    workspace
                        .update(cx, |this, cx| {
                            range
                                .filter_map(|index| {
                                    let path = this.tabs.paths().get(index)?;
                                    let label = labels.get(index)?;
                                    Some(this.open_file_row(path, label, popup.clone(), cx, colors))
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .w(width)
                .h(height)
                .track_scroll(&scroll);
                div()
                    .relative()
                    .w(width)
                    .h(height)
                    .child(list)
                    .child(div().absolute().inset_0().child(popup_scrollbar(&scroll)))
            })
    }

    fn open_file_row(
        &self,
        path: &Path,
        label: &str,
        popup: WeakEntity<PopoverState>,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement + use<> {
        let active = self.tabs.active() == Some(path);
        let preview = self.tabs.is_preview(path);
        let select = path.to_owned();
        let path_text = paths::display(path);
        let row_group = SharedString::from(format!("open-file-row:{path_text}"));
        let row = h_flex()
            .id(SharedString::from(path_text.clone()))
            .group(row_group.clone())
            .w_full()
            .h(px(46.))
            .px_2()
            .rounded(px(6.))
            .hover(|style| style.bg(colors.hover))
            .when(active, |row| row.bg(colors.accent.opacity(0.15)))
            .child(
                v_flex()
                    .id("select-open-file")
                    .role(Role::Button)
                    .aria_label(path_text.clone())
                    .tab_index(0)
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, {
                        let workspace = cx.weak_entity();
                        move |_, window, cx| {
                            _ = popup.update(cx, |popup, cx| popup.dismiss(window, cx));
                            _ = workspace
                                .update(cx, |this, cx| this.activate_tab(&select, window, cx));
                        }
                    })
                    .child(
                        h_flex()
                            .min_w_0()
                            .text_size(px(13.))
                            .gap(px(3.))
                            .child(file_type_icon(path, colors, 14.))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .when(preview, Styled::italic)
                                    .child(label.to_owned()),
                            )
                            .when(
                                self.tab_is_dirty(path) || self.tab_needs_attention(path),
                                |row| row.child(dirty_mark(colors, self.tab_needs_attention(path))),
                            )
                            .when(path.is_absolute(), |row| {
                                row.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_color(colors.muted)
                                        .child(t!("tabs.local_file")),
                                )
                            })
                            .when(preview, |row| {
                                row.child(div().flex_shrink_0().italic().child(t!("tabs.preview")))
                            }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(colors.muted)
                            .child(path_text),
                    ),
            )
            .child(tab_close_button(
                path,
                cx.weak_entity(),
                active,
                colors,
                row_group,
            ));
        self.tab_context_menu(row, path, cx)
    }

    fn tab_context_menu(
        &self,
        element: Stateful<Div>,
        path: &Path,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let path = path.to_owned();
        let workspace = cx.weak_entity();
        element
            .context_menu(move |mut menu, _, cx| {
                let (has_others, has_right, has_saved) = workspace
                    .update(cx, |this, _| {
                        let paths = this.tabs.paths();
                        (
                            paths.len() > 1,
                            this.tabs
                                .index(&path)
                                .is_some_and(|index| index + 1 < paths.len()),
                            paths.iter().any(|path| !this.tab_is_dirty(path)),
                        )
                    })
                    .unwrap_or_default();
                for (action, label, enabled) in [
                    (TabClose::This, t!("common.close"), true),
                    (TabClose::Others, t!("tabs.close_others"), has_others),
                    (TabClose::Right, t!("tabs.close_right"), has_right),
                    (TabClose::Saved, t!("tabs.close_saved"), has_saved),
                ] {
                    let path = path.clone();
                    let workspace = workspace.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .when(matches!(action, TabClose::This), |item| {
                                item.icon(IconName::X)
                            })
                            .disabled(!enabled)
                            .on_click(move |_, window, cx| {
                                _ = workspace.update(cx, |this, cx| {
                                    let paths = this.tabs.paths();
                                    let Some(index) = this.tabs.index(&path) else {
                                        return;
                                    };
                                    let closing = paths
                                        .iter()
                                        .enumerate()
                                        .filter(|(i, candidate)| match action {
                                            TabClose::This => *i == index,
                                            TabClose::Others => *i != index,
                                            TabClose::Right => *i > index,
                                            TabClose::Saved => !this.tab_is_dirty(candidate),
                                        })
                                        .map(|(_, path)| path.clone())
                                        .collect();
                                    this.close_tabs(closing, window, cx);
                                });
                            }),
                    );
                }
                menu = menu.separator();
                if docs::is_markdown(&path) {
                    let target = path.clone();
                    let workspace = workspace.clone();
                    menu = menu.item(
                        PopupMenuItem::new(t!("tabs.open_in_docs"))
                            .icon(IconName::BookOpen)
                            .on_click(move |_, window, cx| {
                                _ = workspace.update(cx, |this, cx| {
                                    this.open_markdown_in_docs(target.clone(), window, cx)
                                });
                            }),
                    );
                }
                for (label, absolute) in [
                    (t!("tabs.copy_relative_path"), false),
                    (t!("tabs.copy_absolute_path"), true),
                ] {
                    if path.is_absolute() && !absolute {
                        continue;
                    }
                    let path = path.clone();
                    let workspace = workspace.clone();
                    menu = menu.item(PopupMenuItem::new(label).icon(IconName::Link2).on_click(
                        move |_, _, cx| {
                            let path = if absolute && !path.is_absolute() {
                                workspace
                                    .read_with(cx, |this, _| {
                                        this.repository.as_ref().map(|repo| repo.root.join(&path))
                                    })
                                    .ok()
                                    .flatten()
                            } else {
                                Some(path.clone())
                            };
                            if let Some(path) = path {
                                cx.write_to_clipboard(ClipboardItem::new_string(paths::display(
                                    &path,
                                )));
                            }
                        },
                    ));
                }
                menu
            })
            .shared_layer()
    }
}

#[derive(Clone, Copy)]
enum TabClose {
    This,
    Others,
    Right,
    Saved,
}

fn dirty_mark(colors: Palette, conflicted: bool) -> Div {
    div()
        .ml(px(3.))
        .flex_shrink_0()
        .size(px(6.))
        .rounded_full()
        .bg(if conflicted {
            colors.warning
        } else {
            colors.accent
        })
}

/// Read-only overflow indicator for the editor tab strip.
///
/// It lives in the 5 px gutter below the 26 px tabs, so its thumb never
/// crosses tab text. The indicator uses scrolling mode, so it fades out once
/// the strip is idle instead of staying visible all the time. A transparent
/// cover keeps the thumb out of hit testing: a press in the gutter cannot
/// start a drag or jump the strip, while wheel and trackpad scrolling still
/// pass through to the strip underneath.
fn tab_overflow_indicator(scroll: &ScrollHandle, colors: Palette) -> impl IntoElement + use<> {
    let handle = scroll.clone();
    div()
        .id("editor-tab-scrollbar")
        .debug_selector(|| "editor-tab-scrollbar".into())
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .h(px(5.))
        .child(
            Scrollbar::horizontal(&handle)
                .viewport_from_layout()
                .mode(ScrollbarMode::Scrolling)
                .styles(|styles| {
                    styles
                        .track(|track| {
                            track
                                .width(px(5.))
                                .bg(transparent_black())
                                .border_color(transparent_black())
                        })
                        .thumb(|thumb| {
                            thumb
                                .width(px(3.))
                                .inset(px(1.))
                                .min_length(px(28.))
                                .radius(px(4.))
                                .bg(colors.muted.opacity(0.14))
                        })
                }),
        )
        .child(
            div()
                .id("editor-tab-scrollbar-cover")
                .absolute()
                .inset_0()
                .block_mouse_except_scroll(),
        )
}

fn tab_close_button(
    path: &Path,
    workspace: WeakEntity<Workspace>,
    active: bool,
    colors: Palette,
    tab_group: SharedString,
) -> impl IntoElement + use<> {
    let close = path.to_owned();
    let path_text = paths::display(path);
    let label = t!("tabs.close_file", path = path_text).into_owned();
    let close_group = SharedString::from(format!("close-tab:{path_text}"));
    let slot_id = SharedString::from(format!("close-tab-slot:{path_text}"));
    let button_id = SharedString::from(format!("close-tab:{path_text}"));
    let icon_id = SharedString::from(format!("close-tab-icon:{path_text}"));
    let icon_focus_id = SharedString::from(format!("close-tab-icon-hover:{path_text}"));

    div()
        .id(slot_id)
        .debug_selector(move || format!("close-tab-slot:{path_text}"))
        .group(close_group.clone())
        .flex_shrink_0()
        .size(px(20.))
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|style| style.bg(colors.danger.opacity(0.12)))
        .child(
            Button::new(button_id)
                .text()
                .size(px(20.))
                .p_0()
                .accessibility_label(label.clone())
                .tooltip(label)
                .tooltip_placement(Placement::Bottom)
                .tooltip_restart_delay()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    _ = workspace.update(cx, |this, cx| {
                        this.close_tabs(vec![close.clone()], window, cx)
                    });
                })
                .child(
                    div()
                        .id(icon_id)
                        .text_color(if active { colors.ink } else { colors.muted })
                        .when(!active, |this| {
                            this.group_hover(tab_group, |style| style.text_color(colors.ink))
                        })
                        .child(
                            div()
                                .id(icon_focus_id)
                                .group_hover(close_group, |style| style.text_color(colors.danger))
                                .child(Icon::new(IconName::X).size(px(14.))),
                        ),
                ),
        )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{fs, path::PathBuf, process::Command, sync::Arc, time::Duration};

    use gpui::{
        Modifiers, MouseButton, ScrollDelta, ScrollEvent, TestAppContext, TestDispatcher,
        VisualTestContext, point, px, size,
    };
    use gpui_component::{Root, ThemeMode};

    use super::*;
    use crate::{
        config::ConfigFile,
        repository::{Repository, Store},
        session::WorkspaceView,
    };

    struct TabStripProbe {
        scroll: ScrollHandle,
    }

    impl Render for TabStripProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let colors = Palette::for_mode(ThemeMode::Light);
            div().w(px(320.)).h(px(36.)).child(
                div()
                    .relative()
                    .w_full()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(
                        h_flex()
                            .id("probe-tab-strip")
                            .w_full()
                            .h(px(26.))
                            .overflow_x_scroll()
                            .lock_scroll_axis()
                            .track_scroll(&self.scroll)
                            .child(
                                h_flex()
                                    .id("probe-tab-text")
                                    .debug_selector(|| "probe-tab-text".into())
                                    .w(px(900.))
                                    .h_full()
                                    .flex_shrink_0(),
                            ),
                    )
                    .child(ScrollableMask::vertical_wheel(&self.scroll))
                    .child(tab_overflow_indicator(&self.scroll, colors)),
            )
        }
    }

    fn draw(visual: &mut VisualTestContext) {
        visual.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
        });
        visual.run_until_parked();
    }

    fn advance(visual: &mut VisualTestContext) {
        visual.executor().advance_clock(Duration::from_millis(16));
        draw(visual);
    }

    #[::core::prelude::v1::test]
    fn overflow_indicator_below_tabs_is_read_only_but_keeps_wheel_scrolling() -> anyhow::Result<()>
    {
        let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let scroll = ScrollHandle::new();
        let handle = scroll.clone();
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            Root::new(cx.new(|_| TabStripProbe { scroll: handle }), window, cx)
        });
        visual.simulate_resize(size(px(320.), px(80.)));
        draw(&mut visual);
        draw(&mut visual);

        let tabs = visual
            .debug_bounds("probe-tab-text")
            .expect("the tab strip lays out its tabs");
        let indicator = visual
            .debug_bounds("editor-tab-scrollbar")
            .expect("overflow renders the indicator");
        assert!(
            indicator.top() >= tabs.bottom(),
            "the indicator must sit below the tab text: {indicator:?} vs {tabs:?}"
        );
        assert_eq!(indicator.size.height, px(5.));
        assert!(scroll.max_offset().x > px(0.));

        // A press in the gutter must not jump or drag the strip.
        let before = scroll.offset();
        let center = indicator.center();
        visual.simulate_mouse_down(center, MouseButton::Left, Modifiers::default());
        visual.simulate_mouse_move(
            center + point(px(-80.), px(0.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        visual.simulate_mouse_up(
            center + point(px(-80.), px(0.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        draw(&mut visual);
        assert_eq!(scroll.offset(), before);

        // The wheel still reaches the strip through the read-only cover.
        visual.simulate_event(ScrollEvent {
            position: center,
            delta: ScrollDelta::Lines(point(0., -3.)),
            ..Default::default()
        });
        for _ in 0..20 {
            advance(&mut visual);
        }
        assert!(scroll.offset().x < before.x);
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn close_hover_uses_the_web_danger_tint() -> anyhow::Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        fs::write(root.join("alpha.rs"), "fn main() {}\n")?;
        fs::write(root.join("beta.rs"), "fn main() {}\n")?;
        let git = |args: &[&str]| -> anyhow::Result<()> {
            let out = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()?;
            anyhow::ensure!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(())
        };
        git(&["init", "-q"])?;
        git(&["add", "."])?;
        git(&[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "init",
        ])?;

        let repo = Repository::open(&root)?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1000.), px(420.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.apply_repository(repo, Default::default(), None, None, window, cx);
            });
        });
        visual.run_until_parked();
        draw(&mut visual);
        for (index, name) in ["alpha.rs", "beta.rs"].into_iter().enumerate() {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.open_file(PathBuf::from(name), false, window, cx);
                    this.open_file(PathBuf::from(name), true, window, cx);
                });
            });
            for _ in 0..100 {
                visual.run_until_parked();
                draw(&mut visual);
                if visual.update(|_, cx| workspace.read(cx).tabs.paths().len()) > index {
                    break;
                }
            }
        }
        draw(&mut visual);

        let slot = visual
            .debug_bounds("close-tab-slot:alpha.rs")
            .expect("the inactive tab lays out its close control");

        visual.simulate_mouse_move(slot.center(), None, Modifiers::default());
        draw(&mut visual);

        let danger = Palette::for_mode(ThemeMode::Light).danger.opacity(0.12);
        let expected: gpui::Background = danger.into();
        let painted = visual.update(|window, _| {
            window
                .painted_quads()
                .iter()
                .any(|quad| quad.background == expected)
        });
        assert!(
            painted,
            "hovering a close control must paint the web danger tint"
        );
        Ok(())
    }

    /// Tabs switch on press so the editor follows the mouse down, not the
    /// release.
    #[::core::prelude::v1::test]
    fn tab_switches_on_mouse_down() -> anyhow::Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        fs::write(root.join("alpha.rs"), "fn main() {}\n")?;
        fs::write(root.join("beta.rs"), "fn main() {}\n")?;
        let git = |args: &[&str]| -> anyhow::Result<()> {
            let out = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()?;
            anyhow::ensure!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(())
        };
        git(&["init", "-q"])?;
        git(&["add", "."])?;
        git(&[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "init",
        ])?;

        let repo = Repository::open(&root)?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1000.), px(420.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.apply_repository(repo, Default::default(), None, None, window, cx);
            });
        });
        visual.run_until_parked();
        draw(&mut visual);
        for (index, name) in ["alpha.rs", "beta.rs"].into_iter().enumerate() {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.open_file(PathBuf::from(name), false, window, cx);
                });
            });
            for _ in 0..100 {
                visual.run_until_parked();
                draw(&mut visual);
                if visual.update(|_, cx| workspace.read(cx).tabs.paths().len()) > index {
                    break;
                }
            }
        }
        draw(&mut visual);
        let active = visual.update(|_, cx| workspace.read(cx).tabs.active().map(Path::to_owned));
        assert_eq!(active, Some(PathBuf::from("beta.rs")));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let beta = Path::new("beta.rs");
                assert!(!this.tabs.is_preview(beta));
                let alpha = Path::new("alpha.rs");
                let editor = this.files[alpha].editor.clone();
                this.open_chosen_local_file(root.join(alpha), window, cx);
                this.open_chosen_local_file(root.join(alpha), window, cx);
                assert_eq!(this.active_file().unwrap().editor, editor);
                assert_eq!(this.tabs.paths().len(), 2);
                this.activate_tab(beta, window, cx);
            });
        });
        draw(&mut visual);

        let select = visual
            .debug_bounds("select-tab:alpha.rs")
            .expect("the inactive tab lays out its select area");
        visual.simulate_mouse_down(select.center(), MouseButton::Left, Modifiers::default());
        let active = visual.update(|_, cx| workspace.read(cx).tabs.active().map(Path::to_owned));
        assert_eq!(
            active,
            Some(PathBuf::from("alpha.rs")),
            "the tab must activate on press, before the button is released"
        );
        Ok(())
    }

    /// The ellipsis menu, not an icon of its own, carries the save entry that
    /// used to sit next to it.
    #[::core::prelude::v1::test]
    fn tab_bar_actions_menu_shows_bound_keys_and_saves_the_open_draft() -> anyhow::Result<()> {
        use gpui_component::input::Search;

        use super::super::{SaveFile, ToggleTerminal};

        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        let file = root.join("alpha.rs");
        fs::write(&file, "fn main() {}\n")?;
        let git = |args: &[&str]| -> anyhow::Result<()> {
            let out = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()?;
            anyhow::ensure!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(())
        };
        git(&["init", "-q"])?;
        git(&["add", "."])?;
        git(&[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "init",
        ])?;

        let repo = Repository::open(&root)?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.bind_keys([
                KeyBinding::new("f9", SaveFile, Some("!Terminal && !Locator")),
                KeyBinding::new("f10", ToggleTerminal, Some("!Locator")),
                KeyBinding::new("f11", Search, Some("Input")),
            ]);
        });
        let mut workspace = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1000.), px(420.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.apply_repository(repo, Default::default(), None, None, window, cx);
            });
        });
        visual.run_until_parked();
        draw(&mut visual);
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_file(PathBuf::from("alpha.rs"), false, window, cx);
            });
        });
        for _ in 0..100 {
            visual.run_until_parked();
            draw(&mut visual);
            if visual.update(|_, cx| workspace.read(cx).active_file().is_some()) {
                break;
            }
        }
        let trigger = visual
            .debug_bounds("workspace-actions-trigger")
            .expect("the tab bar lays out the actions trigger");

        // The first entry saves the open draft.
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let file = this.active_file().expect("open file");
                file.editor
                    .clone()
                    .update(cx, |editor, cx| editor.select_all(window, cx));
            });
        });
        visual.simulate_input("rewritten\n");
        // The editor's search binding is resolved even if the file tree had focus.
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| this.focus_tree(PathBuf::new(), window, cx));
        });
        draw(&mut visual);
        visual.simulate_click(trigger.center(), Default::default());
        draw(&mut visual);
        let keys = ["kbd:f9", "kbd:f11", "kbd:f10"].map(|id| {
            visual
                .debug_bounds(id)
                .expect("menu shows the active shortcut")
        });
        assert!(keys[0].top() < keys[1].top() && keys[1].top() < keys[2].top());
        visual.simulate_keystrokes("down enter");
        for _ in 0..100 {
            visual.run_until_parked();
            draw(&mut visual);
            if fs::read_to_string(&file)? == "rewritten\n" {
                break;
            }
        }
        assert_eq!(
            fs::read_to_string(&file)?,
            "rewritten\n",
            "the menu's Save entry must write the draft"
        );
        Ok(())
    }
}
