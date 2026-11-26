use std::time::Duration;

use anyhow::Result;
use gpui::{
    Action, App, Context, InteractiveElement as _, IntoElement, KeyBinding, Keystroke, Modifiers,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Task, Window,
    deferred, div, px,
};
use gpui_base::{MODAL_POPUP_PRIORITY, Root, RootPlugin};
use gpui_component::{StyledExt as _, ThemeStyled as _, h_flex};
use rust_i18n::t;

use crate::config::{UI_SCALE_DEFAULT, UI_SCALE_MAX, UI_SCALE_MIN};

use super::{Workspace, settings::SettingsPage, toast};

const STEP: u16 = 10;

#[derive(Action, Clone, Copy, PartialEq)]
#[action(namespace = workspace, no_json, no_register)]
enum AdjustUiScale {
    Decrease,
    Increase,
    Reset,
}

const SHORTCUTS: [(&str, &str, &str, AdjustUiScale); 3] = [
    ("-", "ctrl--", "cmd--", AdjustUiScale::Decrease),
    ("=", "ctrl-=", "cmd-=", AdjustUiScale::Increase),
    ("0", "ctrl-0", "cmd-0", AdjustUiScale::Reset),
];

pub(super) fn is_shortcut(key: &Keystroke) -> bool {
    let modifiers = Modifiers {
        platform: cfg!(target_os = "macos"),
        control: !cfg!(target_os = "macos"),
        ..Modifiers::default()
    };
    key.modifiers == modifiers && SHORTCUTS.iter().any(|(name, _, _, _)| key.key == *name)
}

pub(super) fn apply(percent: u16, window: &mut Window, cx: &mut App) {
    window.set_ui_scale(f32::from(percent) / f32::from(UI_SCALE_DEFAULT), cx);
}

pub(super) fn apply_and_notify(percent: u16, window: &mut Window, cx: &mut App) {
    let before = window.ui_scale();
    apply(percent, window, cx);
    if window.ui_scale() != before
        && window.is_window_active()
        && let Some(root) = window.root::<Root>().flatten()
        && let Some(popup) = root.read(cx).plugin::<ScalePopup>()
    {
        popup.update(cx, |popup, cx| popup.show(percent, cx));
    }
}

const POPUP_TIMEOUT: Duration = Duration::from_secs(5);

struct ScalePopup {
    percent: Option<u16>,
    hide: Task<()>,
}

impl RootPlugin for ScalePopup {}

impl ScalePopup {
    fn new(_: &mut Window, _: &mut Context<Self>) -> Self {
        Self {
            percent: None,
            hide: Task::ready(()),
        }
    }

    fn show(&mut self, percent: u16, cx: &mut Context<Self>) {
        self.percent = Some(percent);
        self.hide = cx.spawn(async move |popup, cx| {
            cx.background_executor().timer(POPUP_TIMEOUT).await;
            _ = popup.update(cx, |popup, cx| {
                popup.percent = None;
                cx.notify();
            });
        });
        cx.notify();
    }
}

impl Render for ScalePopup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(percent) = self.percent else {
            return div().into_any_element();
        };
        deferred(
            h_flex()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(24.))
                .justify_center()
                .child(
                    div()
                        .id("ui-scale-popup")
                        .debug_selector(|| "ui-scale-popup".into())
                        .aria_label(t!("workspace.ui_scale", percent = percent))
                        .popover_style(cx)
                        .min_w(px(60.))
                        .px_3()
                        .py_1()
                        .text_center()
                        .text_sm()
                        .font_semibold()
                        .child(format!("{percent}%")),
                ),
        )
        .with_priority(MODAL_POPUP_PRIORITY + 1)
        .into_any_element()
    }
}

impl Workspace {
    pub fn bind_ui_scale_keys(cx: &mut App) {
        Root::register_plugin(cx, ScalePopup::new);
        cx.bind_keys(SHORTCUTS.map(|(_, control, command, action)| {
            let key = if cfg!(target_os = "macos") {
                command
            } else {
                control
            };
            KeyBinding::new(key, action, None)
        }));
        cx.on_action(|action: &AdjustUiScale, cx| {
            let Some(target) = cx.active_window() else {
                return;
            };
            let action = *action;
            cx.defer(move |cx| {
                _ = target.update(cx, |root, window, cx| {
                    let Ok(root) = root.downcast::<Root>() else {
                        return;
                    };
                    let view = root.read(cx).view().clone();
                    let result = if let Ok(workspace) = view.clone().downcast::<Workspace>() {
                        workspace.update(cx, |workspace, cx| {
                            workspace.adjust_ui_scale(action, window, cx)
                        })
                    } else if let Ok(settings) = view.downcast::<SettingsPage>() {
                        let (owner, workspace) = settings.read(cx).workspace();
                        owner
                            .update(cx, |_, window, cx| {
                                workspace.update(cx, |workspace, cx| {
                                    workspace.adjust_ui_scale(action, window, cx)
                                })?
                            })
                            .and_then(|result| result)
                    } else {
                        return;
                    };
                    if let Err(error) = result {
                        toast::error(window, cx, format!("{error:#}"));
                    }
                });
            });
        });
    }

    fn adjust_ui_scale(
        &mut self,
        action: AdjustUiScale,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let mut config = self.config.current().clone();
        let current = config.appearance.ui_scale_percent;
        config.appearance.ui_scale_percent = match action {
            AdjustUiScale::Increase => (current + STEP).min(UI_SCALE_MAX),
            AdjustUiScale::Decrease => current.saturating_sub(STEP).max(UI_SCALE_MIN),
            AdjustUiScale::Reset => UI_SCALE_DEFAULT,
        };
        self.commit_settings(config, window, cx)
    }
}
