use std::{
    cell::{OnceCell, RefCell},
    rc::Weak,
};

use gpui_component::{
    ElementExt as _, ThemeStyled as _,
    popover::PopoverState,
    tooltip::{ManagedTooltipExt as _, TooltipRequest},
};

use super::*;

type CardBounds = Cell<Option<Bounds<Pixels>>>;

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
        trigger.managed_interactive_tooltip(move |bounds, _, _| {
            let info = self.clone();
            let hover = hover.clone();
            let view = OnceCell::new();
            Some(
                TooltipRequest::new(bounds, move |window, cx| {
                    view.get_or_init(|| {
                        let card = Card::new(info.clone(), None, window, cx);
                        *hover.0.borrow_mut() = Rc::downgrade(&card.read(cx).bounds);
                        card
                    })
                    .clone()
                    .into()
                })
                .with_placement(Placement::Right)
                .with_restart_delay(true),
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
            let file = self.clone();
            let view = OnceCell::new();
            Some(
                TooltipRequest::new(bounds, move |window, cx| {
                    view.get_or_init(|| file.build(window, cx)).clone().into()
                })
                .with_placement(placement)
                .with_restart_delay(true)
                .paint_only(),
            )
        })
    }

    pub(super) fn build(&self, window: &Window, cx: &mut App) -> Entity<Card> {
        let Ok((info, repo)) = self.workspace.read_with(cx, |workspace, _| {
            (
                workspace.file_hover_info(&self.path),
                workspace.repository.clone(),
            )
        }) else {
            return Card::new(
                Info::new(file_name(&self.path).to_owned()),
                None,
                window,
                cx,
            );
        };
        Card::new(info, Some((repo, self.path.clone())), window, cx)
    }
}

impl Workspace {
    fn file_hover_info(&self, path: &Path) -> Info {
        let mut info = Info::new(file_name(path).to_owned());
        info.row(t!("hover.path"), paths::display(path));
        let kind: SharedString = if crate::image_viewer::is_image(path) {
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
        if let Some(file) = self.files.get(path) {
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
    embedded: bool,
    bounds: Rc<CardBounds>,
    _metadata: Task<()>,
}

impl Card {
    fn new(
        info: Info,
        file: Option<(Option<Arc<Repository>>, PathBuf)>,
        window: &Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let width = px(320.).min((window.viewport_size().width - px(16.)).max(px(1.)));
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
                                let date: chrono::DateTime<chrono::Local> = modified.into();
                                this.info.row(
                                    t!("hover.modified"),
                                    date.format("%Y-%m-%d %H:%M:%S %:z").to_string(),
                                );
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
                embedded: false,
                bounds: Rc::default(),
                _metadata: metadata.unwrap_or_else(|| Task::ready(())),
            }
        })
    }
}

impl Card {
    pub(super) fn embed(&mut self, width: Pixels) {
        self.width = width;
        self.embedded = true;
    }
}

impl Render for Card {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .id("hover-info")
            .debug_selector(|| "hover-info".into())
            .w(self.width)
            .gap(px(5.))
            .p_3()
            .text_sm()
            .when(!self.embedded, |card| card.popover_style(cx))
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
