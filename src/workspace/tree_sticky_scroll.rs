use gpui_component::tooltip::{ManagedTooltipExt as _, Tooltip};

use super::{tree_motion::ROW_HEIGHT, *};

const MAX_ROWS: usize = 3;

struct PinnedRow {
    index: usize,
    height: Pixels,
}

pub(super) fn row_limit(viewport_height: Pixels) -> usize {
    ((viewport_height / (ROW_HEIGHT * 3.)) as usize).min(MAX_ROWS)
}

fn pinned_rows(
    rows: &[TreeRow],
    frame: Option<&tree_motion::TreeFrame<TreeRow>>,
    scroll_top: Pixels,
    viewport_height: Pixels,
) -> Vec<PinnedRow> {
    let row_top = |index| match frame {
        Some(frame) if index == rows.len() => Some(frame.height()),
        Some(frame) => frame.row_top(index),
        None => Some(ROW_HEIGHT * index),
    };
    let limit = row_limit(viewport_height);
    let mut pinned: Vec<PinnedRow> = Vec::with_capacity(limit);
    let mut ancestors = Vec::new();
    for slot in 0..limit {
        let edge = scroll_top + ROW_HEIGHT * slot;
        let index = match frame {
            Some(frame) => frame.row_at(edge),
            None => Some((edge / ROW_HEIGHT) as usize),
        };
        let Some(index) = index.filter(|index| *index < rows.len()) else {
            break;
        };
        let row = &rows[index];
        let mut parent = if row.subtree_end > index + 1 && row_top(index).is_some_and(|y| y < edge)
        {
            Some(index)
        } else {
            row.parent
        };
        ancestors.clear();
        while let Some(index) = parent {
            ancestors.push(index);
            parent = rows[index].parent;
        }
        // Read the ancestors from shallowest to deepest. Once the stack has
        // started, a directory from another branch cannot join it.
        let candidate = if let Some(previous) = pinned.last() {
            ancestors
                .iter()
                .position(|&index| index == previous.index)
                .and_then(|position| position.checked_sub(1))
                .and_then(|position| ancestors.get(position))
        } else {
            ancestors.get(ancestors.len().min(limit).saturating_sub(1))
        };
        let Some(&index) = candidate else { break };
        let Some(bottom) = row_top(rows[index].subtree_end) else {
            break;
        };
        let height = (bottom - edge).min(ROW_HEIGHT);
        if height <= px(0.) {
            break;
        }
        pinned.push(PinnedRow { index, height });
        if height < ROW_HEIGHT {
            break;
        }
    }
    pinned
}

pub(super) fn overlay(
    rows: Rc<Vec<TreeRow>>,
    frame: Option<Rc<tree_motion::TreeFrame<TreeRow>>>,
    scroll: ScrollHandle,
    workspace: WeakEntity<Workspace>,
    active: Option<PathBuf>,
    in_workspace: bool,
    colors: Palette,
    gradient: Option<PaneGradient>,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            // The list prepaints first, so this includes wheel, scrollbar,
            // deferred jumps, and any clamping after a resize.
            let pinned = pinned_rows(
                &rows,
                frame.as_deref(),
                -scroll.offset().y,
                bounds.size.height,
            );
            let omitted_ancestors = rows[pinned.first()?.index].depth > 0;
            let tree = workspace.clone();
            let mut overlay = v_flex()
                .relative()
                .w_full()
                .bg(colors.bg)
                .when_some(gradient.clone(), |overlay, gradient| {
                    overlay.child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, (), window, _| gradient.paint(bounds, window),
                        )
                        .absolute()
                        .inset_0(),
                    )
                })
                .block_mouse_except_scroll()
                .capture_any_mouse_down(move |event, window, cx| {
                    if let MouseButton::Navigate(direction) = event.button {
                        _ = tree.update(cx, |this, cx| {
                            if this.scroll_tree_with_side_button(
                                direction,
                                event.position,
                                window,
                                cx,
                            ) {
                                window.prevent_default();
                                cx.stop_propagation();
                            }
                        });
                    }
                });
            for (slot, pinned) in pinned.into_iter().enumerate() {
                let row = &rows[pinned.index];
                let selected = active.as_ref() == Some(&row.entry.path);
                let element = directory_row(
                    row,
                    slot == 0 && omitted_ancestors,
                    selected,
                    &workspace,
                    colors,
                    cx,
                );
                let element = element.top(pinned.height - ROW_HEIGHT);
                let menu = if in_workspace {
                    file_tree::tree_menu
                } else {
                    file_tree::document_tree_menu
                };
                overlay = overlay.child(
                    div()
                        .w_full()
                        .h(pinned.height)
                        .overflow_hidden()
                        .child(menu(element, Some(row.entry.clone()), workspace.clone())),
                );
            }
            let black = Hsla::black();
            let opacity = if cx.theme().mode.is_dark() {
                0.24
            } else {
                0.07
            };
            let shadow = linear_gradient(
                180.,
                linear_color_stop(black.opacity(opacity), 0.),
                linear_color_stop(black.opacity(0.), 1.),
            );
            let mut overlay = overlay
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, (), window, _| window.paint_quad(fill(bounds, shadow)),
                    )
                    .absolute()
                    .top_full()
                    .left_0()
                    .w_full()
                    .h(px(5.)),
                )
                .into_any_element();
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                overlay.prepaint_as_root(
                    bounds.origin,
                    bounds.size.map(AvailableSpace::Definite),
                    window,
                    cx,
                );
            });
            Some(overlay)
        },
        |bounds, overlay, window, cx| {
            if let Some(mut overlay) = overlay {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    overlay.paint(window, cx);
                });
            }
        },
    )
    .absolute()
    .inset_0()
}

fn directory_row(
    row: &TreeRow,
    omitted_ancestors: bool,
    selected: bool,
    workspace: &WeakEntity<Workspace>,
    colors: Palette,
    cx: &App,
) -> Stateful<Div> {
    let path = &row.entry.path;
    let full_path = paths::display(path);
    let name = file_name(path);
    let label = if omitted_ancestors {
        format!("…/{name}")
    } else {
        name.to_owned()
    };
    let icon_color = file_tree::directory_icon_color(row, selected, colors);
    let collapse_id = format!("workspace-tree-sticky-collapse:{full_path}");
    let jump_id = format!("workspace-tree-sticky-row:{full_path}");
    let collapsed = path.clone();
    let jumped = path.clone();
    let collapse_target = workspace.clone();
    let jump_target = workspace.clone();

    file_tree::tree_row(
        format!("workspace-tree-sticky:{full_path}"),
        row,
        selected,
        colors,
    )
    .child(
        h_flex()
            .id(SharedString::from(collapse_id.clone()))
            .debug_selector(move || collapse_id.clone())
            .h_full()
            .flex_shrink_0()
            .cursor_pointer()
            .child(folder_icons::folder_icon(path, true, icon_color, colors))
            .on_click(move |_, window, cx| {
                _ = collapse_target.update(cx, |this, cx| {
                    this.navigate_sticky_directory(&collapsed, true, window, cx)
                });
            }),
    )
    .child(
        h_flex()
            .id(SharedString::from(jump_id.clone()))
            .debug_selector(move || jump_id.clone())
            .h_full()
            .flex_1()
            .min_w_0()
            .cursor_pointer()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(file_name_font_size(cx)))
                    .when(row.dimmed && !selected, |name| {
                        name.text_color(colors.muted)
                    })
                    .child(label),
            )
            .managed_tooltip(move |window, cx| Tooltip::new(full_path.clone()).build(window, cx))
            .on_click(move |_, window, cx| {
                _ = jump_target.update(cx, |this, cx| {
                    this.navigate_sticky_directory(&jumped, false, window, cx)
                });
            }),
    )
}

impl Workspace {
    fn navigate_sticky_directory(
        &mut self,
        path: &Path,
        collapse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .rows
            .iter()
            .position(|row| row.expanded && row.entry.path == path)
        else {
            return;
        };
        self.focus_tree(path.to_owned(), window, cx);
        if collapse {
            self.toggle_tree_directory(path.to_owned(), window, cx);
        }
        self.tree_scroll
            .scroll_to_item_strict(index, ScrollStrategy::Center);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn sticky_scroll_keeps_the_deepest_directories_within_the_height_limit() {
        let rows: Vec<_> = (0usize..16)
            .map(|index| TreeRow {
                entry: Entry {
                    path: PathBuf::new(),
                    directory: index < 5,
                    symlink: false,
                    ignored: false,
                },
                depth: index.min(5),
                parent: index.min(5).checked_sub(1),
                subtree_end: if index < 5 { 16 } else { index + 1 },
                expanded: index < 5,
                dimmed: false,
                guides: file_tree::TreeGuides::default(),
            })
            .collect();
        let indices = |height| {
            pinned_rows(&rows, None, ROW_HEIGHT * 6., height)
                .into_iter()
                .map(|row| row.index)
                .collect::<Vec<_>>()
        };
        assert!(indices(ROW_HEIGHT * 2.9).is_empty());
        assert_eq!(indices(ROW_HEIGHT * 6.), [3, 4]);
        assert_eq!(indices(ROW_HEIGHT * 20.), [2, 3, 4]);
    }
}
