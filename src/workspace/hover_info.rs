use std::{
    cell::{OnceCell, RefCell},
    rc::Weak,
};

use gpui_component::{
    ElementExt as _, ThemeStyled as _,
    popover::{Popover, PopoverState},
    tooltip::{ManagedTooltipExt as _, Positioner, TooltipRequest},
};

use super::*;

type CardBounds = Cell<Option<Bounds<Pixels>>>;

#[derive(Clone, Copy)]
pub(super) struct ArrowHover {
    side: Placement,
    target: Point<Pixels>,
    anchor: Anchor,
    origin: Point<Pixels>,
    pub(super) width: Pixels,
    pub(super) height: Pixels,
}

impl ArrowHover {
    pub(super) fn new(bounds: Bounds<Pixels>, preferred: Placement, window: &Window) -> Self {
        Self::with_gap(bounds, preferred, px(10.), false, window)
    }

    fn with_gap(
        bounds: Bounds<Pixels>,
        preferred: Placement,
        gap: Pixels,
        prefer_right: bool,
        window: &Window,
    ) -> Self {
        const MARGIN: Pixels = px(8.);
        const INSET: Pixels = px(18.);
        let viewport = Positioner::viewport_bounds(window, MARGIN);
        let right = viewport.right() - bounds.right() - gap;
        let left = bounds.left() - viewport.left() - gap;
        let above = bounds.top() - viewport.top() - gap;
        let below = viewport.bottom() - bounds.bottom() - gap;
        let side = if preferred.is_horizontal() && right.max(left) >= px(160.) {
            if (prefer_right && right >= px(160.)) || right >= px(320.) || right >= left {
                Placement::Right
            } else {
                Placement::Left
            }
        } else if below >= px(240.) || below >= above {
            Placement::Bottom
        } else {
            Placement::Top
        };
        let center = bounds.center();
        let (target, anchor, origin, width, height) = match side {
            Placement::Right => (
                point(bounds.right(), center.y),
                Anchor::TopLeft,
                point(bounds.right() + gap, center.y - INSET),
                right,
                viewport.size.height,
            ),
            Placement::Left => (
                point(bounds.left(), center.y),
                Anchor::TopRight,
                point(bounds.left() - gap, center.y - INSET),
                left,
                viewport.size.height,
            ),
            Placement::Bottom => (
                point(center.x, bounds.bottom()),
                Anchor::TopLeft,
                point(center.x - INSET, bounds.bottom() + gap),
                viewport.size.width,
                below,
            ),
            Placement::Top => (
                point(center.x, bounds.top()),
                Anchor::BottomLeft,
                point(center.x - INSET, bounds.top() - gap),
                viewport.size.width,
                above,
            ),
        };
        Self {
            side,
            target,
            anchor,
            origin,
            width: width.min(px(320.)).max(px(1.)),
            height,
        }
    }

    pub(super) fn positioner(self) -> Positioner {
        Positioner::corner(self.anchor, self.origin).margin(px(8.))
    }

    pub(super) fn surface(self, window: &Window, cx: &App) -> Div {
        Popover::surface_with_arrow(self.side, self.target, window, cx).p_0()
    }
}

pub(super) fn modified_label(time: std::time::SystemTime) -> String {
    let date: chrono::DateTime<chrono::Local> = time.into();
    date.format("%Y-%m-%d %H:%M:%S %:z").to_string()
}

#[derive(Clone, Default)]
pub(super) struct BranchHover(Rc<RefCell<Weak<CardBounds>>>);

impl BranchHover {
    pub(super) fn dismiss_menu_on_outside(
        &self,
        menu: Stateful<Div>,
        state: WeakEntity<PopoverState>,
    ) -> Stateful<Div> {
        let hover = self.clone();
        // The shared tooltip is drawn outside the menu's own hitbox.
        menu.on_mouse_down_out(move |event, window, cx| {
            let inside_card = hover
                .0
                .borrow()
                .upgrade()
                .and_then(|bounds| bounds.get())
                .is_some_and(|bounds| bounds.contains(&event.position));
            if !inside_card {
                _ = state.update(cx, |state, cx| state.dismiss(window, cx));
            }
        })
    }
}

#[derive(Clone)]
pub(super) struct Info {
    pub(super) title: SharedString,
    pub(super) badge: Option<SharedString>,
    pub(super) rows: Vec<(SharedString, SharedString)>,
    pub(super) branch_to_copy: Option<SharedString>,
}

impl Info {
    pub(super) fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            badge: None,
            rows: Vec::new(),
            branch_to_copy: None,
        }
    }

    pub(super) fn row(&mut self, label: impl Into<SharedString>, value: impl Into<SharedString>) {
        self.rows.push((label.into(), value.into()));
    }

    pub(super) fn attach(self, trigger: Stateful<Div>, hover: BranchHover) -> Stateful<Div> {
        trigger.managed_tooltip_request(false, move |bounds, window, _| {
            let arrow = ArrowHover::new(bounds, Placement::Right, window);
            let info = self.clone();
            let hover = hover.clone();
            let view = OnceCell::new();
            Some(
                TooltipRequest::new(bounds, move |_, cx| {
                    view.get_or_init(|| {
                        let card = Card::new(info.clone(), None, Some(arrow), cx);
                        *hover.0.borrow_mut() = Rc::downgrade(&card.read(cx).bounds);
                        card
                    })
                    .clone()
                    .into()
                })
                .with_positioner(move || arrow.positioner())
                .interactive(),
            )
        })
    }

    pub(super) fn commit(&mut self, oid: Option<&str>) {
        if let Some(oid) = oid {
            self.row(t!("hover.commit"), oid.chars().take(7).collect::<String>());
        }
    }
}

#[derive(Clone)]
pub(super) struct FileInfo {
    workspace: WeakEntity<Workspace>,
    path: PathBuf,
    tree_edge: Option<Rc<CardBounds>>,
}

impl FileInfo {
    pub(super) fn new(workspace: WeakEntity<Workspace>, path: PathBuf) -> Self {
        Self {
            workspace,
            path,
            tree_edge: None,
        }
    }

    pub(super) fn at_tree_edge(mut self, edge: Rc<CardBounds>) -> Self {
        self.tree_edge = Some(edge);
        self
    }

    pub(super) fn arrow(
        &self,
        row: Bounds<Pixels>,
        placement: Placement,
        window: &Window,
    ) -> ArrowHover {
        if let Some(tree) = self.tree_edge.as_ref().and_then(|edge| edge.get()) {
            let bounds = Bounds::new(
                point(tree.left(), row.top()),
                size(tree.size.width, row.size.height),
            );
            // Allow for the standard popover arrow before the visible gap.
            return ArrowHover::with_gap(
                bounds,
                placement,
                window.rem_size() * 0.375 + px(3.),
                true,
                window,
            );
        }
        ArrowHover::new(row, placement, window)
    }

    pub(super) fn attach(self, trigger: Stateful<Div>, placement: Placement) -> Stateful<Div> {
        trigger.managed_tooltip_request(true, move |bounds, window, _| {
            if !window.is_pointer_motion() {
                return None;
            }
            let arrow = self.arrow(bounds, placement, window);
            let file = self.clone();
            let view = OnceCell::new();
            Some(
                TooltipRequest::new(bounds, move |_, cx| {
                    view.get_or_init(|| file.build(Some(arrow), cx))
                        .clone()
                        .into()
                })
                .with_positioner(move || arrow.positioner())
                .paint_only(),
            )
        })
    }

    pub(super) fn build(&self, arrow: Option<ArrowHover>, cx: &mut App) -> Entity<Card> {
        let Ok((info, repo)) = self.workspace.read_with(cx, |workspace, _| {
            (
                workspace.file_hover_info(&self.path),
                workspace.repository.clone(),
            )
        }) else {
            return Card::new(Info::new(file_name(&self.path).to_owned()), None, arrow, cx);
        };
        Card::new(info, Some((repo, self.path.clone())), arrow, cx)
    }
}

impl Workspace {
    fn file_hover_info(&self, path: &Path) -> Info {
        let mut info = Info::new(file_name(path).to_owned());
        info.row(t!("hover.path"), paths::display(path));
        let kind: SharedString = if let Some(kind) = self
            .file_formats
            .get(path)
            .and_then(|format| format.binary_label())
        {
            kind
        } else if crate::image_viewer::is_image(path) {
            t!("hover.image").into()
        } else if audio_preview::is_audio(path) {
            t!("hover.audio").into()
        } else {
            self.associated_highlight(path)
                .map(|(id, _)| SharedString::from(id.to_owned()))
                .or_else(|| self.source_resources(path).display_name_for_path(path))
                .unwrap_or_else(|| {
                    let language = language_for(path);
                    if language == "text" && !self.files.contains_key(path) {
                        path.extension()
                            .and_then(|extension| extension.to_str())
                            .unwrap_or(t!("hover.file").as_ref())
                            .to_owned()
                            .into()
                    } else {
                        language.into()
                    }
                })
        };
        info.row(t!("hover.file_type"), kind);
        if let Some(encoding) = self
            .file_information
            .get(path)
            .and_then(|details| file_details::encoding(details.issue))
        {
            info.row(t!("hover.encoding"), encoding);
        } else if let Some(file) = self.files.get(path) {
            info.row(
                t!("hover.encoding"),
                if file.document.has_bom() {
                    "UTF-8 BOM"
                } else {
                    "UTF-8"
                },
            );
            info.row(
                t!("hover.line_endings"),
                if file.document.mixed_newlines {
                    t!("hover.mixed_line_endings").into()
                } else {
                    SharedString::from(file.document.line_ending_label())
                },
            );
        }
        info
    }
}

pub(super) struct Card {
    info: Info,
    width: Pixels,
    copied: bool,
    copy_feedback: Task<()>,
    thumbnail: Option<(Arc<RenderImage>, Size<Pixels>)>,
    dimensions: Option<SharedString>,
    arrow: Option<ArrowHover>,
    bounds: Rc<CardBounds>,
    _metadata: Task<()>,
}

impl Card {
    fn new(
        info: Info,
        file: Option<(Option<Arc<Repository>>, PathBuf)>,
        arrow: Option<ArrowHover>,
        cx: &mut App,
    ) -> Entity<Self> {
        let width = arrow.map_or(px(320.), |arrow| arrow.width);
        cx.new(|cx| {
            let metadata = file.map(|(repo, path)| {
                let load = cx.background_spawn(async move {
                    crate::repository::file_details(repo.as_deref(), &path)
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(details) = load.await {
                        _ = this.update(cx, |this: &mut Self, cx| {
                            this.info
                                .row(t!("hover.size"), t!("hover.bytes", count = details.size));
                            if let Some(modified) = details.modified {
                                this.info
                                    .row(t!("hover.modified"), modified_label(modified));
                            }
                            cx.notify();
                        });
                    }
                })
            });
            Self {
                info,
                width,
                copied: false,
                copy_feedback: Task::ready(()),
                thumbnail: None,
                dimensions: None,
                arrow,
                bounds: Rc::default(),
                _metadata: metadata.unwrap_or_else(|| Task::ready(())),
            }
        })
    }
}

impl Card {
    pub(super) fn set_preview(
        &mut self,
        original: Size<u32>,
        thumbnail: Option<(Arc<RenderImage>, Size<Pixels>)>,
    ) {
        self.dimensions = Some(format!("{} × {}", original.width, original.height).into());
        self.thumbnail = thumbnail;
    }
}

struct CopyBranchTooltip;

impl Render for CopyBranchTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("copy-branch-tooltip")
            .role(Role::Tooltip)
            .popover_style(cx)
            .m_2()
            .px_2()
            .py_0p5()
            .text_sm()
            .whitespace_nowrap()
            .child(
                div()
                    .debug_selector(|| "copy-branch-tooltip-label".into())
                    .child(t!("hover.copy_branch")),
            )
    }
}

impl Render for Card {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let embedded = self.arrow.is_none();
        self.arrow
            .map_or_else(v_flex, |arrow| arrow.surface(window, cx))
            .relative()
            .id("hover-info")
            .debug_selector(|| "hover-info".into())
            .when(!embedded, |card| card.w(self.width).gap(px(5.)).p_3())
            .when(embedded, |card| {
                card.w_full()
                    .gap(px(2.))
                    .px(px(3.))
                    .line_height(px(16.))
                    .flex_shrink_0()
            })
            .text_sm()
            .when_some(self.arrow, |card, arrow| card.max_h(arrow.height))
            .on_prepaint({
                let bounds = self.bounds.clone();
                move |area, _, _| bounds.set(Some(area))
            })
            .child(
                h_flex()
                    .gap_2()
                    .when_some(self.thumbnail.clone(), |header, (image, size)| {
                        header.child(
                            div()
                                .debug_selector(|| "image-hover-preview".into())
                                .flex_shrink_0()
                                .child(super::image_preview::preview_image(image, size)),
                        )
                    })
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .child(div().font_semibold().child(self.info.title.clone()))
                            .children(
                                self.dimensions
                                    .iter()
                                    .map(|size| div().text_color(muted).child(size.clone())),
                            ),
                    )
                    .when(self.info.branch_to_copy.is_some(), |header| {
                        header.child(
                            div()
                                .id("branch-copy-control")
                                .capture_any_mouse_down(|event, window, _| {
                                    if event.button == MouseButton::Left {
                                        window.prevent_default();
                                    }
                                })
                                .debug_selector(|| "hover-copy-branch".into())
                                // A managed tooltip here would replace the interactive branch card.
                                .tooltip(|_, cx| cx.new(|_| CopyBranchTooltip).into())
                                .child(
                                    compact_icon_control(
                                        "hover-copy-branch",
                                        if self.copied {
                                            IconName::Check
                                        } else {
                                            IconName::Copy
                                        },
                                        t!("hover.copy_branch"),
                                    )
                                    .flex_shrink_0()
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            if let Some(branch) = &this.info.branch_to_copy {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    branch.to_string(),
                                                ));
                                                this.copied = true;
                                                let timer = cx
                                                    .background_executor()
                                                    .timer(Duration::from_millis(1500));
                                                this.copy_feedback =
                                                    cx.spawn(async move |this, cx| {
                                                        timer.await;
                                                        _ = this.update(cx, |this, cx| {
                                                            this.copied = false;
                                                            cx.notify();
                                                        });
                                                    });
                                                cx.notify();
                                                cx.stop_propagation();
                                            }
                                        },
                                    )),
                                ),
                        )
                    }),
            )
            .children(
                self.info
                    .badge
                    .clone()
                    .map(|badge| div().text_color(muted).child(badge)),
            )
            .children(self.info.rows.iter().map(|(label, value)| {
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .when(!embedded, |label| label.w(px(82.)))
                            .flex_shrink_0()
                            .text_color(muted)
                            .child(label.clone()),
                    )
                    .child(div().min_w_0().flex_1().child(value.clone()))
            }))
    }
}
