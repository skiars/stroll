use std::{
    cell::{OnceCell, RefCell},
    rc::Weak,
};

use gpui_component::{
    ElementExt as _,
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
        const MARGIN: Pixels = px(8.);
        const GAP: Pixels = px(10.);
        const INSET: Pixels = px(18.);
        let viewport = Positioner::viewport_bounds(window, MARGIN);
        let right = viewport.right() - bounds.right() - GAP;
        let left = bounds.left() - viewport.left() - GAP;
        let above = bounds.top() - viewport.top() - GAP;
        let below = viewport.bottom() - bounds.bottom() - GAP;
        let side = if preferred.is_horizontal() && right.max(left) >= px(160.) {
            if right >= px(320.) || right >= left {
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
                point(bounds.right() + GAP, center.y - INSET),
                right,
                viewport.size.height,
            ),
            Placement::Left => (
                point(bounds.left(), center.y),
                Anchor::TopRight,
                point(bounds.left() - GAP, center.y - INSET),
                left,
                viewport.size.height,
            ),
            Placement::Bottom => (
                point(center.x, bounds.bottom()),
                Anchor::TopLeft,
                point(center.x - INSET, bounds.bottom() + GAP),
                viewport.size.width,
                below,
            ),
            Placement::Top => (
                point(center.x, bounds.top()),
                Anchor::BottomLeft,
                point(center.x - INSET, bounds.top() - GAP),
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
}

impl FileInfo {
    pub(super) fn new(workspace: WeakEntity<Workspace>, path: PathBuf) -> Self {
        Self { workspace, path }
    }

    pub(super) fn attach(self, trigger: Stateful<Div>, placement: Placement) -> Stateful<Div> {
        trigger.managed_tooltip_request(true, move |bounds, window, _| {
            if !window.is_pointer_motion() {
                return None;
            }
            let arrow = ArrowHover::new(bounds, placement, window);
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
        let kind: SharedString =
            if self.file_formats.get(path) == Some(&file_icons::KnownFormat::Binary) {
                t!("file_details.binary_type").into()
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
                arrow,
                bounds: Rc::default(),
                _metadata: metadata.unwrap_or_else(|| Task::ready(())),
            }
        })
    }
}

impl Card {
    pub(super) fn embed(&mut self, width: Pixels) {
        self.width = width;
        self.arrow = None;
    }
}

impl Render for Card {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        self.arrow
            .map_or_else(v_flex, |arrow| arrow.surface(window, cx))
            .relative()
            .id("hover-info")
            .debug_selector(|| "hover-info".into())
            .w(self.width)
            .gap(px(5.))
            .p_3()
            .text_sm()
            .when_some(self.arrow, |card, arrow| card.max_h(arrow.height))
            .when(self.arrow.is_none(), |card| card.flex_shrink_0())
            .on_prepaint({
                let bounds = self.bounds.clone();
                move |area, _, _| bounds.set(Some(area))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .font_semibold()
                            .child(self.info.title.clone()),
                    )
                    .when(self.info.branch_to_copy.is_some(), |header| {
                        header.child(
                            div()
                                .capture_any_mouse_down(|event, window, _| {
                                    if event.button == MouseButton::Left {
                                        window.prevent_default();
                                    }
                                })
                                .debug_selector(|| "hover-copy-branch".into())
                                .child(
                                    quiet_button("hover-copy-branch")
                                        .small()
                                        .flex_shrink_0()
                                        .label(if self.copied {
                                            t!("git.blame_copied")
                                        } else {
                                            t!("hover.copy_branch")
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(branch) = &this.info.branch_to_copy {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    branch.to_string(),
                                                ));
                                                this.copied = true;
                                                cx.notify();
                                                cx.stop_propagation();
                                            }
                                        })),
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
                            .w(px(82.))
                            .flex_shrink_0()
                            .text_color(muted)
                            .child(label.clone()),
                    )
                    .child(div().min_w_0().flex_1().child(value.clone()))
            }))
    }
}
