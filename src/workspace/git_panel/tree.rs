use rust_i18n::t;

use std::{collections::BTreeMap, path::Path};

use gpui_component::{
    checkbox::Checkbox,
    collapsible::Collapsible,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
};

use super::{
    review::{change_stat, diff_counts, status_badge},
    *,
};
use crate::workspace::{
    baseline_aligned, file_name_font_size,
    tree_motion::{self, ROW_HEIGHT as TREE_ROW_HEIGHT},
};

/// Width of the clickable staging slot, slightly wider than the 16 px checkbox.
const STAGE_SLOT_WIDTH: f32 = 24.;
const HUNK_PANEL_MAX_HEIGHT: Pixels = px(190.);

fn hunk_count(count: usize) -> Cow<'static, str> {
    if count == 1 {
        t!("git.hunk_count_one", count = count)
    } else {
        t!("git.hunk_count_other", count = count)
    }
}

#[derive(Default)]
struct Directory {
    children: BTreeMap<String, Directory>,
    files: Vec<usize>,
    matches_filter: bool,
}

#[derive(Clone)]
pub(super) enum Row {
    Folder {
        path: String,
        label: String,
        depth: usize,
        closed: bool,
        files: Rc<[usize]>,
    },
    File {
        ix: usize,
        depth: usize,
    },
}

pub(super) fn matches(file: &git::ChangedFile, filter: &str) -> bool {
    filter.is_empty()
        || file.path.to_lowercase().contains(filter)
        || file
            .old_path
            .as_ref()
            .is_some_and(|old| old.to_lowercase().contains(filter))
}

impl Directory {
    fn from_files(files: &[git::ChangedFile], matching: &[usize]) -> Self {
        let mut root = Self::default();
        for (ix, file) in files.iter().enumerate() {
            let matches = matching.binary_search(&ix).is_ok();
            let mut current = &mut root;
            if let Some((parent, _)) = file.path.rsplit_once('/') {
                for part in parent.split('/') {
                    current = current.children.entry(part.to_owned()).or_default();
                    current.matches_filter |= matches;
                }
            }
            current.files.push(ix);
        }
        root
    }

    fn all_files(&self) -> impl Iterator<Item = usize> + '_ {
        fn collect(dir: &Directory, files: &mut Vec<usize>) {
            for child in dir.children.values() {
                collect(child, files);
            }
            files.extend(&dir.files);
        }
        let mut files = Vec::new();
        collect(self, &mut files);
        files.into_iter()
    }

    fn rows(
        &self,
        prefix: &str,
        depth: usize,
        closed: &BTreeSet<String>,
        force_open: bool,
        matching: &[usize],
        rows: &mut Vec<Row>,
    ) {
        for (segment, dir) in &self.children {
            if !dir.matches_filter {
                continue;
            }
            let mut label = segment.clone();
            let mut path = if prefix.is_empty() {
                segment.clone()
            } else {
                format!("{prefix}/{segment}")
            };
            let mut current = dir;
            while current.files.is_empty() && current.children.len() == 1 && !closed.contains(&path)
            {
                let (part, child) = current.children.first_key_value().unwrap();
                label.push('/');
                label.push_str(part);
                path.push('/');
                path.push_str(part);
                current = child;
            }
            let is_closed = !force_open && closed.contains(&path);
            rows.push(Row::Folder {
                path: path.clone(),
                label,
                depth,
                closed: is_closed,
                files: current.all_files().collect(),
            });
            if !is_closed {
                current.rows(&path, depth + 1, closed, force_open, matching, rows);
            }
        }
        rows.extend(
            self.files
                .iter()
                .filter(|ix| matching.binary_search(ix).is_ok())
                .map(|&ix| Row::File { ix, depth }),
        );
    }
}

pub(super) struct TreeRows {
    filter: String,
    rows: Rc<Vec<Row>>,
    positions: Vec<Option<usize>>,
    file_rows: Vec<Option<usize>>,
    matching_count: usize,
}

impl TreeRows {
    #[cfg(test)]
    pub(super) fn file_row(&self, file: usize) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::File { ix, .. } if *ix == file))
    }

    fn build(review: &Review, filter: &str) -> Self {
        let mut positions = vec![None; review.detail.files.len()];
        let matching = review
            .detail
            .files
            .iter()
            .enumerate()
            .filter_map(|(ix, file)| {
                if !matches(file, filter) {
                    return None;
                }
                Some(ix)
            })
            .collect::<Vec<_>>();
        for (position, &ix) in matching.iter().enumerate() {
            positions[ix] = Some(position + 1);
        }
        let mut rows = Vec::new();
        Directory::from_files(&review.detail.files, &matching).rows(
            "",
            0,
            &review.directories,
            !filter.is_empty(),
            &matching,
            &mut rows,
        );
        let mut file_rows = vec![None; review.detail.files.len()];
        for (row, entry) in rows.iter().enumerate() {
            if let Row::File { ix, .. } = entry {
                file_rows[*ix] = Some(row);
            }
        }
        Self {
            filter: filter.to_owned(),
            rows: Rc::new(rows),
            positions,
            file_rows,
            matching_count: matching.len(),
        }
    }

    /// Keep the top surviving row in place as the staged/unstaged filter changes membership.
    pub(super) fn preserve_scroll(
        previous: &Review,
        review: &Review,
        filter: &str,
        scroll: &UniformListScrollHandle,
    ) {
        let old = previous.tree_rows.borrow();
        let Some(old) = old.as_ref().filter(|rows| rows.filter == filter) else {
            return;
        };
        let handle = &scroll.0.borrow().base_handle;
        let offset = handle.offset();
        if offset.y >= px(0.) {
            return;
        }
        let top = ((-offset.y / TREE_ROW_HEIGHT) as usize).min(old.rows.len().saturating_sub(1));
        let rows = Self::build(review, filter);
        let identity = |row: &Row, review: &Review| match row {
            Row::Folder { path, .. } => (true, path.clone()),
            Row::File { ix, .. } => (false, review.detail.files[*ix].path.clone()),
        };
        let positions = rows
            .rows
            .iter()
            .enumerate()
            .map(|(ix, row)| (identity(row, review), ix))
            .collect::<HashMap<_, _>>();
        let nearest = old
            .rows
            .iter()
            .skip(top)
            .chain(old.rows[..top].iter().rev())
            .find_map(|row| positions.get(&identity(row, previous)).copied());
        if let Some(nearest) = nearest {
            handle.set_offset(point(
                offset.x,
                offset.y + TREE_ROW_HEIGHT * (top as f32 - nearest as f32),
            ));
        }
        *review.tree_rows.borrow_mut() = Some(rows);
    }
}

impl GitPanel {
    pub(super) fn tree_panel(
        &mut self,
        width: Pixels,
        colors: Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let review = self.review.as_ref().unwrap();
        let active = self.active_file(cx);
        let (rows, position, matching_count, target) = {
            let mut cache = review.tree_rows.borrow_mut();
            if cache.as_ref().is_none_or(|rows| rows.filter != self.filter) {
                *cache = Some(TreeRows::build(review, &self.filter));
            }
            let cached = cache.as_ref().unwrap();
            (
                cached.rows.clone(),
                active
                    .and_then(|ix| cached.positions[ix])
                    .map_or_else(|| "–".into(), |ix| ix.to_string()),
                cached.matching_count,
                active.and_then(|ix| {
                    cached.file_rows[ix].map(|row| (review.detail.files[ix].path.as_str(), row))
                }),
            )
        };
        let path = target.map(|(path, _)| path);
        if self.tree_followed_file.as_deref() != path {
            self.tree_followed_file = path.map(str::to_owned);
            if let Some((_, row)) = target {
                self.tree_scroll
                    .scroll_to_item(row, ScrollStrategy::Nearest);
            }
        }
        let frame = review.tree_motion.borrow_mut().frame(
            &rows,
            |row| match row {
                Row::Folder { path, .. } => Ok(path.clone()),
                Row::File { ix, .. } => Err(*ix),
            },
            self.filter.is_empty(),
            window,
            cx,
        );
        let selected_row = target.and_then(|(_, row)| {
            let top = match &frame {
                Some(frame) => frame.row_top(row)?,
                None => TREE_ROW_HEIGHT * row,
            };
            Some((row, top))
        });
        let total = review.detail.files.len();
        let count = rows.len();
        let detail = review.detail.clone();
        let counts = review.counts.clone();
        let stats = review.stats.clone();
        let hunks = active
            .map(|ix| review.documents[ix].hunk_locations().collect::<Vec<_>>())
            .unwrap_or_default();
        let stage_allowed = detail.status.is_some() && self.current_worktree();
        let stage_busy = self.writes_busy(cx);
        let view = cx.weak_entity();
        let workspace = self.workspace.clone();
        let mode = detail.comparison.worktree_mode().unwrap_or(Mode::All);
        let historical = detail.comparison.commit_target().is_some() || mode == Mode::Staged;
        let rows = frame.as_ref().map_or(rows, |frame| frame.rows.clone());
        let list = tree_motion::tree_list(
            cx.entity(),
            "git-changed-tree",
            count,
            &self.tree_scroll,
            frame,
            move |index, visual, cx| {
                let stage_checkbox = |path: &str, files: &[usize], directory: bool| {
                    let stage = stage_paths(&detail, files, true, stage_allowed);
                    let unstage = stage_paths(&detail, files, false, stage_allowed);
                    let Some(state) = staging_state(&stage, &unstage) else {
                        return div()
                            .w(px(STAGE_SLOT_WIDTH))
                            .flex_shrink_0()
                            .into_any_element();
                    };
                    let staged = match mode {
                        Mode::All => state != StagingState::Staged,
                        Mode::Staged => false,
                        Mode::Unstaged => true,
                    };
                    let (action, paths) = if staged {
                        (git::FileAction::Stage, stage)
                    } else {
                        (git::FileAction::Unstage, unstage)
                    };
                    let ready = !paths.is_empty()
                        && workspace
                            .read_with(cx, |workspace, _| {
                                paths.iter().all(|path| {
                                    workspace
                                        .check_git_file(std::path::Path::new(path), action)
                                        .is_ok()
                                })
                            })
                            .unwrap_or(false);
                    let status = match state {
                        StagingState::Unstaged => t!("git.unstaged"),
                        StagingState::Mixed => t!("git.partially_staged"),
                        StagingState::Staged => t!("git.staged"),
                    };
                    let intent = if !ready {
                        t!("git.save_before_git")
                    } else if directory {
                        t!(
                            "git.stage_folder_tooltip",
                            action = action.label(),
                            count = paths.len()
                        )
                    } else {
                        action.label()
                    };
                    let tooltip = format!("{status} · {intent}");
                    let disabled = stage_busy || !ready || visual.exiting;
                    // The entire slot handles staging clicks, including the space around the 16 px checkbox.
                    // Clicks on the rest of the row still open the file or toggle the directory.
                    let slot_action: Rc<dyn Fn(&mut Window, &mut App)> = {
                        let view = view.clone();
                        let paths = paths.clone();
                        Rc::new(move |window, cx| {
                            cx.stop_propagation();
                            _ = view.update(cx, |panel, cx| {
                                panel.stage_files(paths.clone(), staged, window, cx);
                            });
                        })
                    };
                    let slot_click = slot_action.clone();
                    let checkbox_click = slot_action.clone();
                    div()
                        .id(SharedString::from(format!("git-tree-stage-slot:{path}")))
                        .debug_selector(|| format!("git-tree-stage:{path}"))
                        .w(px(STAGE_SLOT_WIDTH))
                        .h_full()
                        .flex_shrink_0()
                        .h_flex()
                        .justify_end()
                        .when(!disabled, |slot| {
                            slot.on_click(move |_, window, cx| slot_click(window, cx))
                        })
                        .child(
                            Checkbox::new(SharedString::from(format!("git-tree-stage:{path}")))
                                .accessibility_label(format!("{path} · {tooltip}"))
                                .tooltip(tooltip)
                                .checked(state == StagingState::Staged)
                                .indeterminate(state == StagingState::Mixed)
                                .indicator_border_color(colors.control)
                                .keep_indicator_color_when_disabled(
                                    ready && (stage_busy || visual.exiting),
                                )
                                .tab_stop(!visual.exiting)
                                .disabled(disabled)
                                .on_click(move |_, window, cx| checkbox_click(window, cx)),
                        )
                        .into_any_element()
                };
                Some(match &rows[index] {
                    Row::Folder {
                        path,
                        label,
                        depth,
                        closed,
                        files,
                    } => {
                        let view = view.clone();
                        let path = path.clone();
                        let directory = path.clone();
                        let menu_detail = detail.clone();
                        let files = files.clone();
                        let context_view = view.clone();
                        let system_workspace = workspace.clone();
                        let file_name_size = file_name_font_size(cx);
                        tree_row(("folder", path.clone()), *depth, colors)
                            .child(icon(
                                if *closed {
                                    IconName::Folder
                                } else {
                                    IconName::FolderOpen
                                },
                                colors.accent,
                                15.,
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_medium()
                                    .text_size(px(file_name_size))
                                    .child(label.clone()),
                            )
                            .child(
                                div()
                                    .text_color(colors.muted)
                                    .child(files.len().to_string()),
                            )
                            .when(stage_allowed, |row| {
                                row.child(stage_checkbox(&path, &files, true))
                            })
                            .on_click(move |_, _, cx| {
                                _ = view.update(cx, |this, cx| {
                                    if let Some(review) = &mut this.review {
                                        let directories = Arc::make_mut(&mut review.directories);
                                        if !directories.remove(&path) {
                                            directories.insert(path.clone());
                                        }
                                        this.directories_changed(cx);
                                    }
                                });
                            })
                            .context_menu(move |mut menu, _, cx| {
                                menu = append_system_menu(
                                    menu,
                                    Path::new(&directory),
                                    true,
                                    historical,
                                    &system_workspace,
                                    cx,
                                );
                                let path = directory.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(t!("git.copy_directory"))
                                        .icon(IconName::Link2)
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                path.clone(),
                                            ));
                                        }),
                                );
                                let mut worktree_items = Vec::new();
                                for (staged, label) in [
                                    (true, t!("git.stage_directory")),
                                    (false, t!("git.unstage_directory")),
                                ] {
                                    let paths =
                                        stage_paths(&menu_detail, &files, staged, stage_allowed);
                                    if !paths.is_empty() {
                                        let view = context_view.clone();
                                        worktree_items.push(
                                            PopupMenuItem::new(label)
                                                .icon(if staged {
                                                    IconName::Plus
                                                } else {
                                                    IconName::Minus
                                                })
                                                .disabled(stage_busy)
                                                .on_click(move |_, window, cx| {
                                                    _ = view.update(cx, |this, cx| {
                                                        this.stage_files(
                                                            paths.clone(),
                                                            staged,
                                                            window,
                                                            cx,
                                                        )
                                                    });
                                                }),
                                        );
                                    }
                                }
                                worktree_items.extend(worktree_restore_items(
                                    &context_view,
                                    &menu_detail,
                                    &files,
                                    &directory,
                                    true,
                                    cx,
                                ));
                                if !worktree_items.is_empty() {
                                    menu = menu.separator();
                                    for item in worktree_items {
                                        menu = menu.item(item);
                                    }
                                }
                                menu
                            })
                    }
                    Row::File { ix, depth } => {
                        let ix = *ix;
                        let file = &detail.files[ix];
                        let stat = change_stat(&stats, file);
                        let path = file.path.clone();
                        let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
                        let select_view = view.clone();
                        let select_path = path.clone();
                        let menu_view = view.clone();
                        let menu_detail = detail.clone();
                        let system_workspace = workspace.clone();
                        let stage = stage_paths(&detail, &[ix], true, stage_allowed);
                        let unstage = stage_paths(&detail, &[ix], false, stage_allowed);
                        let file_name_size = file_name_font_size(cx);
                        tree_row(("file", path.clone()), *depth, colors)
                            .when(active == Some(ix), |row| {
                                row.bg(colors.accent.opacity(0.13))
                                    .text_color(colors.accent)
                            })
                            .child(status_badge(file, colors, file_name_size, cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(file_name_size))
                                    .child(name),
                            )
                            .when_some(counts.get(&ix).copied().or(stat.counts), |row, counts| {
                                row.child(baseline_aligned(
                                    cx,
                                    GIT_BADGE_SIZE.as_f32(),
                                    file_name_size,
                                    diff_counts(counts, GIT_BADGE_SIZE, cx),
                                ))
                            })
                            .when(stat.binary, |row| {
                                row.child(div().text_color(colors.muted).child(t!("git.binary")))
                            })
                            .when(stage_allowed, |row| {
                                row.child(stage_checkbox(&path, &[ix], false))
                            })
                            .on_click(move |_, window, cx| {
                                _ = select_view.update(cx, |this, cx| {
                                    this.reveal_tree_file(&select_path, window, cx)
                                });
                            })
                            .context_menu(move |mut menu, _, cx| {
                                let can_open = menu_view
                                    .read_with(cx, |panel, _| {
                                        panel.openable_review_file(&path).is_some()
                                    })
                                    .unwrap_or(false);
                                let view = menu_view.clone();
                                let editor_path = path.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(t!("git.open_in_editor"))
                                        .icon(IconName::FileInput)
                                        .disabled(!can_open)
                                        .on_click(move |_, window, cx| {
                                            _ = view.update(cx, |panel, cx| {
                                                panel.open_diff_file_in_editor(
                                                    &editor_path,
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }),
                                );
                                if can_open && crate::docs::is_markdown(std::path::Path::new(&path))
                                {
                                    let view = menu_view.clone();
                                    let docs_path = path.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(t!("tabs.open_in_docs"))
                                            .icon(IconName::BookOpen)
                                            .on_click(move |_, window, cx| {
                                                _ = view.update(cx, |panel, cx| {
                                                    panel
                                                        .open_diff_markdown(&docs_path, window, cx);
                                                });
                                            }),
                                    );
                                }
                                menu = append_system_menu(
                                    menu,
                                    Path::new(&path),
                                    false,
                                    historical,
                                    &system_workspace,
                                    cx,
                                );
                                let copy_path = path.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(t!("git.copy_file_path"))
                                        .icon(IconName::Link2)
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_path.clone(),
                                            ));
                                        }),
                                );
                                let mut worktree_items = Vec::new();
                                for (paths, staged, label) in [
                                    (&stage, true, t!("git.stage_file")),
                                    (&unstage, false, t!("git.unstage")),
                                ] {
                                    if !paths.is_empty() {
                                        let view = menu_view.clone();
                                        let paths = paths.clone();
                                        worktree_items.push(
                                            PopupMenuItem::new(label)
                                                .icon(if staged {
                                                    IconName::Plus
                                                } else {
                                                    IconName::Minus
                                                })
                                                .disabled(stage_busy)
                                                .on_click(move |_, window, cx| {
                                                    _ = view.update(cx, |this, cx| {
                                                        this.stage_files(
                                                            paths.clone(),
                                                            staged,
                                                            window,
                                                            cx,
                                                        )
                                                    });
                                                }),
                                        );
                                    }
                                }
                                worktree_items.extend(worktree_restore_items(
                                    &menu_view,
                                    &menu_detail,
                                    &[ix],
                                    &path,
                                    false,
                                    cx,
                                ));
                                if !worktree_items.is_empty() {
                                    menu = menu.separator();
                                    for item in worktree_items {
                                        menu = menu.item(item);
                                    }
                                }
                                menu
                            })
                    }
                })
            },
        );

        v_flex()
            .id("git-file-index")
            .relative()
            .w(width)
            .h_full()
            .flex_shrink_0()
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px_3()
                    .pb_2()
                    .gap_2()
                    .child(
                        h_flex()
                            .debug_selector(|| "git-tree-toolbar".into())
                            .h(super::GIT_TOOLBAR_HEIGHT)
                            .flex_shrink_0()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_semibold()
                                    .text_size(pane_title_size(cx))
                                    .truncate()
                                    .child(t!("git.files_index")),
                            )
                            .child(baseline_aligned(
                                cx,
                                GIT_TEXT_SIZE.as_f32(),
                                pane_title_size(cx).as_f32(),
                                div()
                                    .text_size(GIT_TEXT_SIZE)
                                    .text_color(colors.muted)
                                    .child(format!("{position}/{}", matching_count)),
                            ))
                            .child(
                                icon_control(
                                    "git-tree-options",
                                    IconName::Ellipsis,
                                    t!("git.filter_and_collapse"),
                                    self.only_matching,
                                )
                                .dropdown_menu({
                                    let view = cx.weak_entity();
                                    let only_matching = self.only_matching;
                                    move |menu, _, _| {
                                        let toggle = view.clone();
                                        let collapse = view.clone();
                                        let expand = view.clone();
                                        let fold = view.clone();
                                        let unfold = view.clone();
                                        menu.item(
                                            PopupMenuItem::new(t!("git.show_matching_files"))
                                                .checked(only_matching)
                                                .on_click(move |_, _, cx| {
                                                    _ = toggle.update(cx, |this, cx| {
                                                        this.only_matching = !this.only_matching;
                                                        this.sync_documents(cx);
                                                        cx.notify();
                                                    });
                                                }),
                                        )
                                        .separator()
                                        .item(
                                            PopupMenuItem::new(t!("git.collapse_directories"))
                                                .on_click(move |_, _, cx| {
                                                    _ = collapse.update(cx, |this, cx| {
                                                        this.set_directories(true, cx)
                                                    });
                                                }),
                                        )
                                        .item(
                                            PopupMenuItem::new(t!("git.expand_directories"))
                                                .on_click(move |_, _, cx| {
                                                    _ = expand.update(cx, |this, cx| {
                                                        this.set_directories(false, cx)
                                                    });
                                                }),
                                        )
                                        .separator()
                                        .item(
                                            PopupMenuItem::new(t!("git.collapse_diffs")).on_click(
                                                move |_, _, cx| {
                                                    _ = fold.update(cx, |this, cx| {
                                                        this.set_files_collapsed(true, cx)
                                                    });
                                                },
                                            ),
                                        )
                                        .item(
                                            PopupMenuItem::new(t!("git.expand_diffs")).on_click(
                                                move |_, _, cx| {
                                                    _ = unfold.update(cx, |this, cx| {
                                                        this.set_files_collapsed(false, cx)
                                                    });
                                                },
                                            ),
                                        )
                                    }
                                }),
                            )
                            .child(
                                icon_control(
                                    "git-close-files",
                                    IconName::PanelRightClose,
                                    t!("git.close_files_index"),
                                    false,
                                )
                                .debug_selector(|| "git-close-files".into())
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.set_tree_visible(false, cx);
                                    },
                                )),
                            ),
                    )
                    .child(
                        div().debug_selector(|| "git-file-filter".into()).child(
                            Input::new(&self.filter_input)
                                .w_full()
                                .cleanable(true)
                                .prefix(icon(IconName::Search, colors.muted, 13.)),
                        ),
                    )
                    .when(matching_count != total, |row| {
                        row.child(
                            div()
                                .text_size(GIT_TEXT_SIZE)
                                .text_color(colors.muted)
                                .child(
                                    t!(
                                        "git.file_filter_count",
                                        matched = matching_count,
                                        total = total
                                    )
                                    .into_owned(),
                                ),
                        )
                    }),
            )
            .when(count == 0, |panel| {
                panel.child(message(
                    if total == 0 {
                        t!("git.no_changed_files")
                    } else {
                        t!("git.no_matching_files")
                    },
                    colors.muted,
                ))
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .when(count > 0, |panel| {
                        panel
                            .when_some(selected_row, |panel, (row, row_top)| {
                                let scroll = self.tree_scroll.clone();
                                panel.child(
                                    canvas(
                                        move |bounds, _, _| {
                                            let handle = scroll.0.borrow().base_handle.clone();
                                            let top = row_top + handle.offset().y;
                                            let bottom = top + TREE_ROW_HEIGHT;
                                            // Keep the selected row visible if the tree shrinks.
                                            if top >= px(0.)
                                                && bottom <= handle.bounds().size.height
                                                && bottom > bounds.size.height
                                            {
                                                scroll.scroll_to_item(row, ScrollStrategy::Nearest);
                                            }
                                        },
                                        |_, _, _, _| (),
                                    )
                                    .absolute()
                                    .inset_0(),
                                )
                            })
                            .child(list)
                            .child(
                                div().absolute().inset_0().child(
                                    crate::workspace::pane_scrollbar(&self.tree_scroll)
                                        .id("git-tree-scroll")
                                        .viewport_from_layout(),
                                ),
                            )
                    }),
            )
            .when(count > 0, |panel| {
                let has_hunks = !hunks.is_empty();
                panel.child(
                    Collapsible::new()
                        .motion_id("git-hunk-list-reveal")
                        .open(self.show_hunks)
                        .when(!has_hunks && !self.show_hunks, |footer| footer.invisible())
                        .w_full()
                        .max_h(HUNK_PANEL_MAX_HEIGHT)
                        .min_h_0()
                        .border_t_1()
                        .border_color(colors.border)
                        .px_2()
                        .py_1()
                        .child(
                            quiet_button("git-toggle-hunks")
                                .small()
                                .w_full()
                                .disabled(!has_hunks && !self.show_hunks)
                                .icon(if self.show_hunks {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .label(
                                    t!("git.current_file_hunks", hunks = hunk_count(hunks.len()))
                                        .into_owned(),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_hunks = !this.show_hunks;
                                    cx.notify();
                                })),
                        )
                        .content(
                            v_flex()
                                .id("git-hunk-list")
                                .w_full()
                                .min_h_0()
                                // Patch loading and file changes share the same open height.
                                .h((HUNK_PANEL_MAX_HEIGHT - window.rem_size() * 2. - px(1.))
                                    .max(px(0.)))
                                .overflow_y_scroll()
                                .when(!self.show_hunks, |list| list.block_mouse_except_scroll())
                                .children(hunks.into_iter().enumerate().map(
                                    |(ix, (label, position))| {
                                        h_flex()
                                            .id(("git-hunk", ix))
                                            .min_h(px(26.))
                                            .px_2()
                                            .gap_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .hover(|row| row.bg(colors.hover))
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(GIT_TEXT_SIZE)
                                                    .text_color(colors.muted)
                                                    .child(label),
                                            )
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                if this.show_hunks {
                                                    this.jump_to_line(position.clone(), window, cx);
                                                }
                                            }))
                                    },
                                )),
                        ),
                )
            })
            .child(panel_resize(
                "git-resize-tree",
                HandleEdge::Leading,
                super::GIT_TOOLBAR_HEIGHT,
            ))
            .child(super::pane_edge(
                HandleEdge::Leading,
                super::GIT_TOOLBAR_HEIGHT,
                cx.theme().mode,
            ))
    }

    fn set_directories(&mut self, closed: bool, cx: &mut Context<Self>) {
        let Some(review) = &mut self.review else {
            return;
        };
        let directories = Arc::make_mut(&mut review.directories);
        if closed {
            let matching = (0..review.detail.files.len()).collect::<Vec<_>>();
            let root = Directory::from_files(&review.detail.files, &matching);
            let mut rows = Vec::new();
            root.rows("", 0, &BTreeSet::new(), true, &matching, &mut rows);
            directories.extend(rows.into_iter().filter_map(|row| match row {
                Row::Folder { path, .. } => Some(path),
                Row::File { .. } => None,
            }));
        } else {
            directories.clear();
        }
        self.directories_changed(cx);
    }

    fn directories_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(review) = &mut self.review {
            review.tree_rows.get_mut().take();
            review.tree_motion.get_mut().start();
            if matches!(
                review.detail.comparison,
                Comparison::Worktree { base: None, .. }
            ) {
                self.worktree_directories.clone_from(&review.directories);
            }
            cx.notify();
        }
    }
}

fn append_system_menu(
    menu: gpui_component::menu::PopupMenu,
    path: &Path,
    directory: bool,
    historical: bool,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> gpui_component::menu::PopupMenu {
    if let Some(workspace) = workspace.upgrade() {
        workspace
            .read(cx)
            .system_tree_menu(menu, path, directory, true, historical)
    } else {
        menu
    }
}

fn tree_row((kind, path): (&str, String), depth: usize, colors: Palette) -> Stateful<Div> {
    let id = format!("git-tree-row:{kind}:{path}");
    h_flex()
        .id(SharedString::from(id.clone()))
        .debug_selector(move || id)
        .h(TREE_ROW_HEIGHT)
        .w_full()
        .gap_1()
        .pl(px(9. + depth.min(14) as f32 * 12.))
        .pr(px(16.))
        .cursor_pointer()
        .hover(|row| row.bg(colors.hover))
        .text_size(GIT_TEXT_SIZE)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StagingState {
    Unstaged,
    Mixed,
    Staged,
}

fn staging_state(stage: &[String], unstage: &[String]) -> Option<StagingState> {
    match (stage.is_empty(), unstage.is_empty()) {
        (false, true) => Some(StagingState::Unstaged),
        (false, false) => Some(StagingState::Mixed),
        (true, false) => Some(StagingState::Staged),
        (true, true) => None,
    }
}

fn stage_paths(detail: &Changes, files: &[usize], staged: bool, allowed: bool) -> Vec<String> {
    if !allowed {
        return Vec::new();
    }
    files
        .iter()
        .filter_map(|&ix| {
            let file = &detail.files[ix];
            let state = file.worktree.as_ref()?;
            (!state.conflict
                && if staged {
                    state.untracked || state.unstaged
                } else {
                    state.staged
                })
            .then(|| file.path.clone())
        })
        .collect()
}

pub(super) fn worktree_restore_items(
    owner: &WeakEntity<GitPanel>,
    detail: &Changes,
    files: &[usize],
    path: &str,
    directory: bool,
    cx: &App,
) -> Vec<PopupMenuItem> {
    if !matches!(
        detail.comparison.worktree_mode(),
        Some(Mode::All | Mode::Unstaged)
    ) {
        return Vec::new();
    }
    owner
        .read_with(cx, |panel, cx| {
            if !panel.current_worktree() || !panel.backend.supports_git_operations() {
                return Vec::new();
            }
            let resolutions = files.iter().filter_map(|&ix| {
                let file = &detail.files[ix];
                panel.stage_resolution_item(&file.path, owner, cx)
            });
            let (untracked, tracked): (Vec<_>, Vec<_>) = files
                .iter()
                .filter_map(|&ix| {
                    let file = &detail.files[ix];
                    let status = file.worktree.as_ref()?;
                    // An intent-to-add file has no staged contents to restore.
                    (!file.submodule()
                        && (status.untracked || status.code != 'A')
                        && git::FileAction::Restore.available(status))
                    .then(|| {
                        (
                            file.path.clone(),
                            status.clone(),
                            !matches(file, &panel.filter),
                        )
                    })
                })
                .partition(|(_, status, _)| status.untracked);
            let busy = panel.writes_busy(cx);
            let restore_label = if directory {
                t!("git.restore_directory")
            } else {
                t!("git.restore_file")
            };
            resolutions
                .chain(
                    [
                        (untracked, t!("git.discard_changes"), IconName::Trash),
                        (tracked, restore_label, IconName::RotateCcw),
                    ]
                    .into_iter()
                    .filter(|(targets, _, _)| !targets.is_empty())
                    .map(|(targets, label, icon)| {
                        let path = path.to_owned();
                        let owner = owner.clone();
                        PopupMenuItem::new(format!("{label}…"))
                            .icon(icon)
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                _ = owner.update(cx, |panel, cx| {
                                    panel.prepare_worktree_restore(
                                        path.clone(),
                                        targets.clone(),
                                        directory,
                                        window,
                                        cx,
                                    );
                                });
                            })
                    }),
                )
                .collect()
        })
        .unwrap_or_default()
}
