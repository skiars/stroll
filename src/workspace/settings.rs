use rust_i18n::t;

use std::{borrow::Cow, rc::Rc, time::Duration};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme, AxisExt as _, Disableable as _, Icon, IndexPath, Root, Side, Sizable as _,
    StyledExt as _, TitleBar,
    accordion::Accordion,
    button::ButtonVariants as _,
    h_flex,
    highlighter::HighlightTheme,
    input::{Input, InputContentType, InputEvent, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    setting::{
        AnySettingField, NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage,
        Settings,
    },
    v_flex, window_corner_radii,
};

use super::{CloseSettings, ToggleSettings, Workspace, action_button, apply_theme_with_fonts};
use crate::{
    config::{
        CodeContrast, CodeTheme, Config, DiffLayout, EDITOR_LINE_HEIGHT_RANGE, FONT_SIZE_MAX,
        FONT_SIZE_MIN, Theme, UI_FONT_SIZE_MAX, UI_FONT_SIZE_MIN,
    },
    language::CommandSource,
    locale::Language,
    lsp::LanguageServer,
};
use gpui_kit_assets::IconName;

const CONTROL_LINE_HEIGHT: Pixels = px(18.);
const DESCRIPTION_FONT_SIZE: Pixels = px(12.);
const DESCRIPTION_LINE_HEIGHT: Pixels = px(17.);
const SAVED_API_KEY_MASK: &str = "••••••••";

/// The code color preview uses the same syntax captures as the editor.
// The Web reading controls use the same sample and token roles.
const CODE_PREVIEW: &[&[(&str, &str)]] = &[
    &[("// Read source without losing meaning.", "comment")],
    &[
        ("interface", "keyword"),
        (" ", ""),
        ("Reader", "type.interface.lsp.1"),
        ("<", ""),
        ("T", "type.parameter.lsp.0"),
        ("> {", ""),
    ],
    &[
        ("  ", ""),
        ("readonly", "keyword.modifier"),
        (" ", ""),
        ("name", "property.lsp.3"),
        (": ", ""),
        ("string", "type.builtin"),
        (";", ""),
    ],
    &[
        ("  ", ""),
        ("read", "function.method.lsp.1"),
        ("(", ""),
        ("input", "variable.special.lsp.0"),
        (": ", ""),
        ("T", "type.parameter.lsp.0"),
        ("): ", ""),
        ("number", "type.builtin"),
        (";", ""),
    ],
    &[("}", "")],
    &[
        ("const", "keyword"),
        (" ", ""),
        ("LIMIT", "constant.lsp.0"),
        (" = ", ""),
        ("42", "number"),
        (";", ""),
    ],
    &[
        ("const", "keyword"),
        (" ", ""),
        ("sample", "variable.lsp.2"),
        (" = [", ""),
        ("\"current\"", "string"),
        (", ", ""),
        ("true", "boolean"),
        (", ", ""),
        ("false", "boolean"),
        ("];", ""),
    ],
    &[
        ("source", "namespace.lsp.0"),
        (".", ""),
        ("read", "function.lsp.0"),
        ("(", ""),
        ("sample", "variable.lsp.2"),
        (");", ""),
    ],
    &[
        ("class", "keyword"),
        (" ", ""),
        ("Source", "type.lsp.1"),
        (" {", ""),
    ],
    &[
        ("  ", ""),
        ("static", "keyword.modifier"),
        (" ", ""),
        ("create", "function.method.lsp.1"),
        ("() { ", ""),
        ("return", "keyword.return"),
        (" ", ""),
        ("new", "keyword.modifier"),
        (" ", ""),
        ("Source", "type.lsp.0"),
        ("(); }", ""),
    ],
    &[
        ("  ", ""),
        ("read", "function.method.lsp.1"),
        ("(", ""),
        ("input", "variable.special.lsp.0"),
        (": ", ""),
        ("string", "type.builtin"),
        (") {", ""),
    ],
    &[
        ("    ", ""),
        ("const", "keyword"),
        (" ", ""),
        ("size", "variable.lsp.2"),
        (" = ", ""),
        ("input", "variable.special.lsp.0"),
        (".", ""),
        ("length", "property.lsp.0"),
        (";", ""),
    ],
    &[
        ("    ", ""),
        ("return", "keyword.return"),
        (" ", ""),
        ("size", "variable.lsp.2"),
        (" > ", ""),
        ("LIMIT", "constant.lsp.0"),
        (" ? ", ""),
        ("0", "number"),
        (" : ", ""),
        ("size", "variable.lsp.2"),
        (";", ""),
    ],
    &[("  }", "")],
    &[("}", "")],
];

pub(super) struct SettingsWindow {
    handle: AnyWindowHandle,
    draft: Entity<Config>,
}

struct SettingsPage {
    owner: WeakEntity<Workspace>,
    owner_window: AnyWindowHandle,
    draft: Entity<Config>,
    focus: FocusHandle,
    title: SharedString,
    ai_base: Entity<InputState>,
    ai_model: Entity<InputState>,
    ai_key: Entity<InputState>,
    ai_saving: bool,
    ai_fetching_models: bool,
    ai_models: Option<(Entity<SelectState<SearchableVec<String>>>, Subscription)>,
    ai_models_error: Option<String>,
    code_preview_open: bool,
    error: Option<String>,
    save_task: Task<()>,
    _subscriptions: [Subscription; 4],
}

#[derive(Clone)]
struct FontChoice {
    family: SharedString,
    default_key: &'static str,
}

impl SelectItem for FontChoice {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        if self.family.is_empty() {
            crate::locale::text(self.default_key).into()
        } else {
            self.family.clone()
        }
    }

    fn value(&self) -> &SharedString {
        &self.family
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .text_size(px(13.5))
            .line_height(CONTROL_LINE_HEIGHT)
            .child(self.title())
    }
}

type FontChoices = SearchableVec<FontChoice>;

impl Theme {
    pub(super) const MODES: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub(super) fn label(self) -> Cow<'static, str> {
        match self {
            Self::System => t!("common.system"),
            Self::Light => t!("settings.light"),
            Self::Dark => t!("settings.dark"),
        }
    }

    pub(super) fn icon(self) -> Icon {
        match self {
            Self::System => Icon::default().path("icons/sun-moon.svg"),
            Self::Light => Icon::new(IconName::Sun),
            Self::Dark => Icon::new(IconName::Moon),
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }
}

impl CodeTheme {
    pub(super) fn label(self) -> Cow<'static, str> {
        match self {
            Self::Prism => t!("settings.code_theme_prism"),
            Self::Orchard => t!("settings.code_theme_orchard"),
            Self::Spectrum => t!("settings.code_theme_spectrum"),
            Self::Neon => t!("settings.code_theme_neon"),
            Self::Lilac => t!("settings.code_theme_lilac"),
            Self::LilacWhite => t!("settings.code_theme_lilac_white"),
            Self::Balanced => t!("settings.code_theme_balanced"),
            Self::Paper => t!("settings.code_theme_paper"),
        }
    }
}

impl CodeContrast {
    pub(super) fn label(self) -> Cow<'static, str> {
        match self {
            Self::Standard => t!("settings.code_contrast_standard"),
            Self::Enhanced => t!("settings.code_contrast_enhanced"),
            Self::High => t!("settings.code_contrast_high"),
        }
    }
}

impl Workspace {
    pub(super) fn toggle_settings(
        &mut self,
        _: &ToggleSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(settings) = &self.settings
            && settings
                .handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }

        let owner = cx.entity();
        window.defer(cx, move |window, cx| {
            if owner
                .read(cx)
                .settings
                .as_ref()
                .is_some_and(|settings| cx.windows().contains(&settings.handle))
            {
                return;
            }
            let draft = cx.new(|cx| owner.read(cx).config.current().clone());
            let mut options = TitleBar::window_options();
            if let Some(titlebar) = &mut options.titlebar {
                titlebar.title = Some(t!("common.settings").into());
            }
            options.app_id = Some("io.stroll.Stroll".into());
            options.window_decorations = Some(WindowDecorations::Client);
            options.window_min_size = Some(size(px(560.), px(400.)));
            options.window_bounds = Some(WindowBounds::Windowed(Bounds::centered(
                window.display(cx).map(|display| display.id()),
                size(px(720.), px(520.)),
                cx,
            )));
            let owner_window = window.window_handle();
            let result = cx.open_window(options, {
                let owner = owner.clone();
                let draft = draft.clone();
                move |window, cx| {
                    let page =
                        cx.new(|cx| SettingsPage::new(owner, owner_window, draft, window, cx));
                    let weak = page.downgrade();
                    window.on_window_should_close(cx, move |_, cx| {
                        _ = weak.update(cx, |page, cx| page.save_now(cx));
                        true
                    });
                    cx.new(|cx| Root::new(page, window, cx))
                }
            });
            match result {
                Ok(handle) => owner.update(cx, |owner, _| {
                    owner.settings = Some(SettingsWindow {
                        handle: handle.into(),
                        draft,
                    });
                }),
                Err(error) => {
                    _ = window.prompt(
                        PromptLevel::Warning,
                        &t!("settings.open_failed"),
                        Some(&format!("{error:#}")),
                        &[PromptButton::new(t!("common.ok"))],
                        cx,
                    );
                }
            }
        });
    }

    pub fn request_window_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.standalone.is_some() {
            if self.confirming_close || !self.saving.is_empty() {
                return false;
            }
            let dirty = self
                .tabs
                .paths()
                .iter()
                .filter(|path| self.tab_is_dirty(path));
            self.close_queue = dirty.cloned().collect();
            if !self.close_queue.is_empty() {
                self.capture_session(cx);
                self.closing_standalone_window = true;
                let previous = self.tabs.active().map(ToOwned::to_owned);
                self.drain_close_queue(previous.as_deref(), window, cx);
                return false;
            }
        }
        if !self.flush_before_close(window, cx) {
            return false;
        }
        if let Some(settings) = self.settings.take()
            && cx.windows().contains(&settings.handle)
        {
            let draft = settings.draft.read(cx).clone();
            _ = self.commit_settings(draft, window, cx);
            _ = settings
                .handle
                .update(cx, |_, window, _| window.remove_window());
        }
        self.audio_preview.stop();
        self.save_window_geometry(window);
        true
    }

    pub(super) fn cycle_theme(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut config = self.config.current().clone();
        config.appearance.theme = config.appearance.theme.next();
        let theme = config.appearance.theme;
        match self.commit_settings(config, window, cx) {
            Ok(()) => {
                if let Some(settings) = &self.settings {
                    settings.draft.update(cx, |draft, cx| {
                        draft.appearance.theme = theme;
                        cx.notify();
                    });
                }
            }
            Err(error) => {
                _ = window.prompt(
                    PromptLevel::Warning,
                    &t!("settings.theme_save_failed"),
                    Some(&format!("{error:#}")),
                    &[PromptButton::new(t!("common.ok"))],
                    cx,
                );
            }
        }
    }

    pub(super) fn commit_settings(
        &mut self,
        draft: Config,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let previous = self.config.current().clone();
        if draft == previous {
            return Ok(());
        }
        for (name, old_name) in [
            (&draft.editor.font_family, &previous.editor.font_family),
            (
                &draft.appearance.ui_font_family,
                &previous.appearance.ui_font_family,
            ),
            (
                &draft.appearance.fallback_font_family,
                &previous.appearance.fallback_font_family,
            ),
        ] {
            if name != old_name && !name.is_empty() && installed_font(name, cx).is_none() {
                anyhow::bail!(t!("settings.font_missing", name = name));
            }
        }
        self.config.save(draft)?;
        self.apply_config(&previous, window, cx);
        Ok(())
    }

    pub(super) fn configured_editor_font(config: &Config, cx: &App) -> Option<SharedString> {
        installed_font(&config.editor.font_family, cx)
    }

    pub(super) fn apply_appearance(&self, window: &Window, cx: &mut App) {
        let config = self.config.current();
        let appearance = &config.appearance;
        let mode = match appearance.theme {
            Theme::System => window.appearance().into(),
            Theme::Light => gpui_component::ThemeMode::Light,
            Theme::Dark => gpui_component::ThemeMode::Dark,
        };
        let ui_font = installed_font(&appearance.ui_font_family, cx);
        let code_font = Self::configured_editor_font(config, cx);
        let fallbacks = installed_font(&appearance.fallback_font_family, cx)
            .map(|family| FontFallbacks::from_fonts(vec![family.to_string()]));
        apply_theme_with_fonts(
            mode,
            px(appearance.ui_font_size as f32),
            ui_font,
            code_font,
            fallbacks,
            appearance.code_theme,
            appearance.code_contrast,
            cx,
        );
        cx.set_smooth_scrolling(appearance.smooth_scroll);
        cx.set_text_rendering_preference(appearance.text_rendering);
    }

    fn apply_config(&mut self, previous: &Config, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.config.current();
        if previous.ai != config.ai {
            self.ai.configure(config.ai.clone());
            for file in self.files.values() {
                file.editor
                    .update(cx, |editor, cx| editor.clear_hover_state(cx));
            }
            if let Some(panel) = &self.git {
                panel.update(cx, |panel, cx| panel.ai_settings_changed(cx));
            }
        }
        let mut previous_appearance = previous.appearance.clone();
        previous_appearance.ui_font_size = config.appearance.ui_font_size;
        if previous_appearance != config.appearance {
            self.apply_appearance(window, cx);
        } else if previous.appearance.ui_font_size != config.appearance.ui_font_size {
            let size = px(config.appearance.ui_font_size as f32);
            gpui_component::Theme::update(cx, |theme| {
                theme.font_size = size;
                theme.control_font_size = size;
            });
        }
        if previous.docs != config.docs
            && let Some(doc) = &self.doc
        {
            doc.update(cx, |reader, cx| reader.set_typography(config.docs, cx));
        }
        if previous.editor.font_family != config.editor.font_family {
            self.editor_font = Self::configured_editor_font(config, cx);
            // Documents and the LSP popovers take their code font from the
            // same setting, so the theme has to be re-applied with it.
            self.apply_appearance(window, cx);
        }
        if previous.editor.line_height != config.editor.line_height
            && let Some(panel) = &self.git
        {
            panel.update(cx, |_, cx| cx.notify());
        }
        if previous.editor.soft_wrap != config.editor.soft_wrap {
            self.wrap = config.editor.soft_wrap;
            for file in self.files.values_mut() {
                file.apply_wrap(self.wrap, window, cx);
            }
        }
        if previous.editor.line_numbers != config.editor.line_numbers {
            for file in self.files.values() {
                file.editor.update(cx, |editor, cx| {
                    editor.set_line_number(config.editor.line_numbers, window, cx)
                });
            }
        }
        if previous.editor.inlay_hints != config.editor.inlay_hints {
            let enabled = config.editor.inlay_hints;
            for file in self.files.values() {
                file.inlay_hints.set_enabled(enabled, cx);
            }
            if let Some(panel) = &self.git {
                panel.update(cx, |panel, cx| {
                    panel.set_inlay_hints_enabled(enabled, window, cx)
                });
            }
        }
        if previous.git != config.git
            && let Some(panel) = &self.git
        {
            panel.update(cx, |panel, cx| {
                panel.apply_settings(previous.git, config.git, window, cx)
            });
        }
        if previous.appearance.language != config.appearance.language
            && config.appearance.language.activate()
        {
            self.repository_ui.refresh_language(window, cx);
            if let Some(reader) = &self.doc {
                reader.update(cx, |reader, cx| reader.refresh_language(window, cx));
            }
            if let Some(panel) = &self.git {
                panel.update(cx, |panel, cx| panel.refresh_language(window, cx));
            }
            cx.refresh_windows();
        }
        cx.notify();
    }
}

fn installed_font(name: &str, cx: &App) -> Option<SharedString> {
    if name.is_empty() {
        return None;
    }
    cx.text_system()
        .all_font_names()
        .into_iter()
        .find(|font| font == name)
        .map(Into::into)
}

impl SettingsPage {
    fn new(
        owner: Entity<Workspace>,
        owner_window: AnyWindowHandle,
        draft: Entity<Config>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let changes = cx.observe_in(&draft, window, |page, _, window, cx| {
            page.save_task = cx.spawn_in(window, async move |page, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(450))
                    .await;
                _ = page.update(cx, |page, cx| page.save_now(cx));
            });
            cx.notify();
        });
        let owner_changes = cx.observe(&owner, |_, _, cx| cx.notify());
        let ai = draft.read(cx).ai.clone();
        let key_placeholder = if ai.has_key_for(&ai.base_url) {
            SAVED_API_KEY_MASK
        } else {
            ""
        };
        let ai_base = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(ai.base_url)
                .placeholder("https://api.openai.com/v1")
        });
        let ai_model = cx.new(|cx| InputState::new(window, cx).default_value(ai.model));
        let ai_key = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(key_placeholder)
        });
        let base_changes = cx.subscribe_in(&ai_base, window, |page, _, event, window, cx| {
            if matches!(event, InputEvent::Change) {
                page.refresh_ai_key_mask(window, cx);
                page.clear_ai_models(cx);
            }
        });
        let key_changes = cx.subscribe(&ai_key, |page, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                page.clear_ai_models(cx);
            }
        });
        Self {
            ai_base,
            ai_model,
            ai_key,
            ai_saving: false,
            ai_fetching_models: false,
            ai_models: None,
            ai_models_error: None,
            owner: owner.downgrade(),
            owner_window,
            draft,
            focus,
            title: t!("common.settings").into(),
            code_preview_open: false,
            error: None,
            save_task: Task::ready(()),
            _subscriptions: [changes, owner_changes, base_changes, key_changes],
        }
    }

    fn save_now(&mut self, cx: &mut Context<Self>) {
        let draft = self.draft.read(cx).clone();
        let result = self.owner_window.update(cx, |_, window, cx| {
            self.owner
                .update(cx, |owner, cx| owner.commit_settings(draft, window, cx))
        });
        self.error = result
            .and_then(|result| result)
            .and_then(|result| result)
            .err()
            .map(|error| t!("settings.save_failed", error = format!("{error:#}")).into_owned());
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_now(cx);
        window.remove_window();
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title: SharedString = t!("common.settings").into();
        if self.title != title {
            window.set_window_title(title.as_ref());
            self.title = title.clone();
        }
        let owner = self.owner.upgrade();
        let workspace = owner.as_ref().map(|owner| owner.read(cx));
        let error = self
            .error
            .clone()
            .or_else(|| workspace.and_then(|owner| owner.config.load_error.clone()));
        let restart =
            workspace.is_some_and(|owner| owner.startup_lsp != owner.config.current().lsp);
        let pages = self.pages(workspace, cx.weak_entity());
        let font_size = px(self.draft.read(cx).appearance.ui_font_size as f32);
        let radii = window_corner_radii(window, cx);
        v_flex()
            .id("settings-window")
            .key_context("SettingsPage")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &CloseSettings, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|_, _: &ToggleSettings, window, _| window.activate_window()))
            .size_full()
            .min_h_0()
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new()
                    .h(px(36.))
                    .child(
                        div()
                            .debug_selector(|| "settings-window-title".into())
                            .px_3()
                            .text_size(font_size)
                            .line_height(CONTROL_LINE_HEIGHT)
                            .font_semibold()
                            .child(title),
                    )
                    .on_close_window(cx.listener(|this, _, window, cx| this.close(window, cx))),
            )
            .child(
                div().flex_1().min_h_0().w_full().child(
                    Settings::new("preferences")
                        .font_size(font_size)
                        .sidebar_width(px(152.))
                        .sidebar_size_range(px(120.)..px(220.))
                        .when(error.is_none() && !restart, |settings| {
                            settings.sidebar_style(
                                &StyleRefinement::default().rounded_bl(radii.bottom_left),
                            )
                        })
                        .pages(pages),
                ),
            )
            .when_some(error, |view, error| {
                view.child(
                    div()
                        .px_3()
                        .py_2()
                        .max_h(px(100.))
                        .id("settings-error")
                        .overflow_y_scroll()
                        .text_size(DESCRIPTION_FONT_SIZE)
                        .line_height(DESCRIPTION_LINE_HEIGHT)
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .when(restart, |view| {
                view.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_size(DESCRIPTION_FONT_SIZE)
                        .line_height(DESCRIPTION_LINE_HEIGHT)
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("settings.lsp_restart")),
                )
            })
    }
}

impl SettingsPage {
    fn pages(&self, workspace: Option<&Workspace>, page: WeakEntity<Self>) -> [SettingPage; 7] {
        let default = Config::default();
        let appearance = SettingGroup::new().items([
            setting_item(t!("settings.theme"), self.theme_picker()),
            setting_item(
                t!("settings.code_theme"),
                self.dropdown(
                    &CodeTheme::ALL.map(|theme| (theme.config_value(), theme.label())),
                    default.appearance.code_theme.config_value(),
                    |c| c.appearance.code_theme.config_value(),
                    |c, value| c.appearance.code_theme = CodeTheme::from_config(value),
                ),
            )
            .description(t!("settings.code_theme_description").into_owned()),
            setting_item(
                t!("settings.code_contrast"),
                self.dropdown(
                    &CodeContrast::ALL.map(|contrast| (contrast.config_value(), contrast.label())),
                    CodeContrast::Enhanced.config_value(),
                    |c| c.appearance.code_contrast.config_value(),
                    |c, value| c.appearance.code_contrast = CodeContrast::from_config(value),
                ),
            )
            .description(t!("settings.code_contrast_description").into_owned()),
            self.code_preview(page.clone()),
            setting_item(
                t!("settings.interface_language"),
                self.dropdown(
                    &[
                        ("system", t!("common.system")),
                        ("zh-CN", t!("settings.language_chinese")),
                        ("en", t!("settings.language_english")),
                    ],
                    Language::System.config_value(),
                    |c| c.appearance.language.config_value(),
                    |c, value| c.appearance.language = Language::from_config(value),
                ),
            )
            .keywords([
                t!("settings.interface_language", locale = "zh-CN"),
                Cow::Borrowed("interface language"),
            ]),
            setting_item(
                t!("settings.ui_font_family"),
                self.font_picker(
                    "ui-font-picker",
                    t!("settings.ui_font_family"),
                    "settings.system_ui_font_family",
                    |c| &mut c.appearance.ui_font_family,
                ),
            )
            .description(t!("settings.ui_font_family_description").into_owned()),
            setting_item(
                t!("settings.ui_font_size"),
                self.number_in(
                    default.appearance.ui_font_size,
                    UI_FONT_SIZE_MIN..=UI_FONT_SIZE_MAX,
                    0.5,
                    |c| &mut c.appearance.ui_font_size,
                ),
            )
            .description(t!("settings.ui_font_size_description").into_owned()),
            setting_item(
                t!("settings.fallback_font_family"),
                self.font_picker(
                    "fallback-font-picker",
                    t!("settings.fallback_font_family"),
                    "settings.system_fallback",
                    |c| &mut c.appearance.fallback_font_family,
                ),
            )
            .description(t!("settings.fallback_font_family_description").into_owned()),
            setting_item(
                t!("settings.smooth_scroll"),
                self.switch(default.appearance.smooth_scroll, |c| {
                    &mut c.appearance.smooth_scroll
                }),
            )
            .description(t!("settings.smooth_scroll_description").into_owned()),
        ]);
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        let appearance = appearance.item(
            setting_item(
                t!("settings.text_rendering"),
                self.dropdown(
                    &[
                        ("auto", t!("settings.auto")),
                        ("pixel_aligned", t!("settings.pixel_aligned")),
                        ("high_precision", t!("settings.high_precision")),
                    ],
                    "auto",
                    |c| match c.appearance.text_rendering {
                        TextRenderingPreference::Auto => "auto",
                        TextRenderingPreference::PixelAligned => "pixel_aligned",
                        TextRenderingPreference::HighPrecision => "high_precision",
                    },
                    |c, value| {
                        c.appearance.text_rendering = match value {
                            "pixel_aligned" => TextRenderingPreference::PixelAligned,
                            "high_precision" => TextRenderingPreference::HighPrecision,
                            _ => TextRenderingPreference::Auto,
                        }
                    },
                ),
            )
            .keywords([Cow::Borrowed("hinting"), t!("settings.hinting")])
            .description(t!("settings.text_rendering_description").into_owned()),
        );
        let docs = SettingGroup::new().items([
            setting_item(
                t!("settings.body_font_size"),
                self.size(default.docs.body_font_size, |c| &mut c.docs.body_font_size),
            ),
            setting_item(
                t!("settings.code_block_font_size"),
                self.size(default.docs.code_block_font_size, |c| {
                    &mut c.docs.code_block_font_size
                }),
            ),
        ]);
        let font_description = if workspace.is_some_and(|workspace| {
            !workspace.config.current().editor.font_family.is_empty()
                && workspace.editor_font.is_none()
        }) {
            t!("settings.missing_mono_font")
        } else {
            t!("settings.filter_fonts")
        };
        let editor = SettingGroup::new().items([
            setting_item(
                t!("settings.code_font_family"),
                self.font_picker(
                    "editor-font-picker",
                    t!("settings.code_font_family"),
                    "settings.system_monospace_font",
                    |c| &mut c.editor.font_family,
                ),
            )
            .description(font_description.into_owned()),
            setting_item(
                t!("settings.code_font_size"),
                self.size(default.editor.font_size, |c| &mut c.editor.font_size),
            ),
            setting_item(
                t!("settings.editor_line_height"),
                self.number_in(
                    default.editor.line_height,
                    EDITOR_LINE_HEIGHT_RANGE,
                    0.1,
                    |c| &mut c.editor.line_height,
                ),
            )
            .description(t!("settings.editor_line_height_description").into_owned()),
            setting_item(
                t!("settings.line_numbers"),
                self.switch(default.editor.line_numbers, |c| &mut c.editor.line_numbers),
            ),
            setting_item(
                t!("settings.editor_wrap"),
                self.switch(default.editor.soft_wrap, |c| &mut c.editor.soft_wrap),
            ),
            setting_item(
                t!("settings.inlay_hints"),
                self.switch(default.editor.inlay_hints, |c| &mut c.editor.inlay_hints),
            )
            .description(t!("settings.inlay_hints_description").into_owned()),
        ]);
        let git = SettingGroup::new().items([
            setting_item(
                t!("settings.diff_layout"),
                self.dropdown(
                    &[
                        ("unified", t!("settings.unified")),
                        ("split", t!("settings.split")),
                    ],
                    "unified",
                    |c| match c.git.diff_layout {
                        DiffLayout::Unified => "unified",
                        DiffLayout::Split => "split",
                    },
                    |c, value| {
                        c.git.diff_layout = if value == "split" {
                            DiffLayout::Split
                        } else {
                            DiffLayout::Unified
                        }
                    },
                ),
            )
            .description(t!("settings.narrow_diff_layout").into_owned()),
            setting_item(
                t!("settings.diff_wrap"),
                self.switch(default.git.soft_wrap, |c| &mut c.git.soft_wrap),
            ),
            setting_item(
                t!("settings.ignore_whitespace"),
                self.switch(default.git.ignore_whitespace, |c| {
                    &mut c.git.ignore_whitespace
                }),
            ),
        ]);
        let terminal = SettingGroup::new().item(
            setting_item(
                t!("settings.default_shell"),
                self.input(|c| &mut c.terminal.shell),
            )
            .description(
                if cfg!(windows) {
                    t!("settings.windows_shell_description")
                } else {
                    t!("settings.shell_description")
                }
                .into_owned(),
            ),
        );
        let mut lsp = SettingGroup::new().item(setting_item(
            t!("settings.enable_lsp"),
            self.switch(default.lsp.enabled, |c| &mut c.lsp.enabled),
        ));
        let startup_lsp = workspace.map(|workspace| &workspace.startup_lsp);
        for (server, title, field) in [
            (
                LanguageServer::Rust,
                "Rust",
                (|c: &mut Config| &mut c.lsp.rust.command) as fn(&mut Config) -> &mut String,
            ),
            (LanguageServer::Clangd, "C / C++ / Objective-C", |c| {
                &mut c.lsp.clangd.command
            }),
            (LanguageServer::TypeScript, "JavaScript / TypeScript", |c| {
                &mut c.lsp.typescript.command
            }),
            (LanguageServer::Python, "Python", |c| {
                &mut c.lsp.python.command
            }),
        ] {
            let (command, source) = server
                .local_command(startup_lsp.map_or("", |lsp| &lsp.server(server).command))
                .expect("startup LSP configuration was validated");
            let words: Vec<_> = std::iter::once(command.get_program())
                .chain(command.get_args())
                .map(|word| word.to_string_lossy())
                .collect();
            let command = shlex::try_join(words.iter().map(|word| word.as_ref()))
                .unwrap_or_else(|_| command.get_program().to_string_lossy().into_owned());
            let source = match source {
                CommandSource::Environment(variable) => Cow::Borrowed(variable),
                CommandSource::Config => t!("settings.lsp_source_config"),
                CommandSource::Path => t!("settings.lsp_source_path"),
            };
            let effective = if startup_lsp.is_none_or(|lsp| lsp.enabled) {
                t!("settings.lsp_command", command = command, source = source).into_owned()
            } else {
                t!(
                    "settings.lsp_disabled_command",
                    command = command,
                    source = source
                )
                .into_owned()
            };
            lsp = lsp.item(
                setting_item(title, self.input(field))
                    .keywords([
                        t!("common.language_server"),
                        Cow::Borrowed("LSP"),
                        t!("settings.lsp_launch_command"),
                    ])
                    .description(effective),
            );
        }
        [
            SettingPage::new(t!("settings.appearance")).group(appearance),
            SettingPage::new(t!("common.docs"))
                .description(t!("settings.font_size_units"))
                .group(docs),
            SettingPage::new(t!("common.editor"))
                .description(t!("settings.font_size_units"))
                .group(editor),
            SettingPage::new("Git").group(git),
            SettingPage::new(t!("common.terminal")).group(terminal),
            SettingPage::new(t!("common.language_server"))
                .description(t!("settings.lsp_description"))
                .group(lsp),
            SettingPage::new(t!("ai.page")).group(
                SettingGroup::new()
                    .item(self.provider_settings(page))
                    .item(setting_item(
                        t!("ai.translate_hover"),
                        self.switch(default.ai.auto_translate, |config| {
                            &mut config.ai.auto_translate
                        }),
                    )),
            ),
        ]
    }

    fn provider_settings(&self, page: WeakEntity<Self>) -> SettingItem {
        let base = self.ai_base.clone();
        let model = self.ai_model.clone();
        let key = self.ai_key.clone();
        let saving = self.ai_saving;
        let fetching = self.ai_fetching_models;
        let busy = saving || fetching;
        let models = self.ai_models.as_ref().map(|(state, _)| state.clone());
        let models_error = self.ai_models_error.clone();
        SettingItem::render(move |_, _, cx| {
            let page = page.clone();
            let fetch_page = page.clone();
            let field = |selector: &'static str, label: SharedString, input| {
                v_flex()
                    .debug_selector(move || selector.into())
                    .gap_1()
                    .child(label)
                    .child(input)
            };
            v_flex()
                .gap_3()
                .child(t!("ai.provider_description").into_owned())
                .child(field(
                    "ai-base-url",
                    t!("ai.base_url").into(),
                    Input::new(&base)
                        .disabled(busy)
                        .aria_label(t!("ai.base_url")),
                ))
                .child(field(
                    "ai-api-key",
                    t!("ai.api_key").into(),
                    Input::new(&key)
                        .disabled(busy)
                        .content_type(InputContentType::Password)
                        .aria_label(t!("ai.api_key")),
                ))
                .child(
                    v_flex()
                        .debug_selector(|| "ai-model-id".into())
                        .gap_1()
                        .child(t!("ai.model"))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Input::new(&model)
                                        .flex_1()
                                        .min_w_0()
                                        .disabled(saving)
                                        .aria_label(t!("ai.model")),
                                )
                                .child(
                                    action_button("ai-fetch-models")
                                        .debug_selector(|| "ai-fetch-models".into())
                                        .secondary()
                                        .small()
                                        .label(t!("ai.fetch_models"))
                                        .loading(fetching)
                                        .disabled(busy)
                                        .on_click(move |_, window, cx| {
                                            _ = fetch_page.update(cx, |page, cx| {
                                                page.fetch_ai_models(window, cx);
                                            });
                                        }),
                                ),
                        )
                        .when_some(models.as_ref(), |view, models| {
                            view.child(
                                div().debug_selector(|| "ai-model-picker".into()).child(
                                    Select::new(models)
                                        .w_full()
                                        .disabled(busy)
                                        .placeholder(t!("ai.choose_model"))
                                        .accessibility_label(t!("ai.choose_model"))
                                        .search_placeholder(t!("ai.search_models")),
                                ),
                            )
                        }),
                )
                .when_some(models_error.clone(), |view, error| {
                    view.child(div().text_color(cx.theme().muted_foreground).child(error))
                })
                .child(
                    div().flex().justify_end().child(
                        action_button("ai-save-provider")
                            .debug_selector(|| "ai-save-provider".into())
                            .secondary()
                            .small()
                            .label(t!("ai.save_provider"))
                            .loading(saving)
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                _ = page.update(cx, |page, cx| page.save_ai_provider(window, cx));
                            }),
                    ),
                )
        })
        .keywords([t!("ai.base_url"), t!("ai.api_key"), t!("ai.model")])
    }

    fn refresh_ai_key_mask(&self, window: &mut Window, cx: &mut Context<Self>) {
        let base = self.ai_base.read(cx).value();
        let placeholder = if self.draft.read(cx).ai.has_key_for(&base) {
            SAVED_API_KEY_MASK
        } else {
            ""
        };
        self.ai_key.update(cx, |state, cx| {
            state.set_placeholder(placeholder, window, cx);
        });
    }

    fn clear_ai_models(&mut self, cx: &mut Context<Self>) {
        self.ai_models = None;
        self.ai_models_error = None;
        cx.notify();
    }

    fn fetch_ai_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_saving || self.ai_fetching_models {
            return;
        }
        self.clear_ai_models(cx);
        let Ok(Ok(scope)) = self
            .owner
            .read_with(cx, |owner, _| owner.config.path().map(|p| p.to_owned()))
        else {
            self.ai_models_error = Some(t!("config.no_directory").into_owned());
            return;
        };
        let base = self.ai_base.read(cx).value().to_string();
        let key = self.ai_key.read(cx).text().to_string();
        let saved = self.draft.read(cx).ai.clone();
        self.ai_fetching_models = true;
        cx.spawn_in(window, async move |this, cx| {
            let result =
                blocking::unblock(move || crate::ai::fetch_models(&scope, &saved, &base, &key))
                    .await;
            _ = this.update_in(cx, |page, window, cx| {
                page.ai_fetching_models = false;
                match result {
                    Ok(models) => {
                        let current = page.ai_model.read(cx).value();
                        let selected = models.iter().position(|model| model == current.as_str());
                        let state = cx.new(|cx| {
                            SelectState::new(
                                SearchableVec::from(models),
                                selected.map(IndexPath::new),
                                window,
                                cx,
                            )
                            .searchable(true)
                        });
                        let subscription =
                            cx.subscribe_in(&state, window, |page, _, event, window, cx| {
                                if let SelectEvent::Confirm(Some(model)) = event {
                                    page.ai_model.update(cx, |input, cx| {
                                        input.set_value(model.as_str(), window, cx);
                                    });
                                }
                            });
                        page.ai_models = Some((state, subscription));
                    }
                    Err(error) => page.ai_models_error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_ai_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_saving || self.ai_fetching_models {
            return;
        }
        let Ok(Ok(scope)) = self
            .owner
            .read_with(cx, |owner, _| owner.config.path().map(|p| p.to_owned()))
        else {
            self.error = Some(t!("config.no_directory").into_owned());
            cx.notify();
            return;
        };
        let base = self.ai_base.read(cx).value().to_string();
        let model = self.ai_model.read(cx).value().to_string();
        let key = self.ai_key.read(cx).text().to_string();
        let previous = self.draft.read(cx).ai.clone();
        let original = previous.clone();
        let worker_scope = scope.clone();
        self.ai_saving = true;
        self.error = None;
        cx.spawn_in(window, async move |this, cx| {
            let saved = blocking::unblock(move || {
                crate::ai::save_provider(&worker_scope, &previous, &base, &model, &key)
            })
            .await;
            let new_settings = saved.as_ref().ok().cloned();
            let committed = this
                .update_in(cx, |page, window, cx| {
                    page.ai_saving = false;
                    let result = saved.and_then(|mut ai| {
                        let mut draft = page.draft.read(cx).clone();
                        ai.auto_translate = draft.ai.auto_translate;
                        draft.ai = ai;
                        page.owner_window.update(cx, |_, window, cx| {
                            page.owner.update(cx, |owner, cx| {
                                owner.commit_settings(draft.clone(), window, cx)
                            })
                        })???;
                        page.draft.update(cx, |draft_config, cx| {
                            *draft_config = draft;
                            cx.notify();
                        });
                        page.ai_key
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        page.refresh_ai_key_mask(window, cx);
                        Ok(())
                    });
                    page.error = result.as_ref().err().map(|err| err.to_string());
                    cx.notify();
                    result.is_ok()
                })
                .unwrap_or(false);
            if let Some(saved) =
                new_settings.filter(|saved| saved.credential != original.credential)
            {
                let obsolete = if committed { original } else { saved };
                blocking::unblock(move || crate::ai::remove_credential(&scope, &obsolete)).await;
            }
        })
        .detach();
        cx.notify();
    }

    /// Highlight the code preview from the settings draft so browsing palettes need not wait for autosave.
    fn code_preview(&self, page: WeakEntity<Self>) -> SettingItem {
        let draft = self.draft.clone();
        let open = self.code_preview_open;
        SettingItem::render(move |_, _, cx| {
            let config = draft.read(cx);
            let appearance = &config.appearance;
            let mode = cx.theme().mode;
            let colors = super::Palette::for_mode(mode);
            let highlight = super::code_theme::highlight_theme(
                appearance.code_theme,
                appearance.code_contrast,
                mode,
                &[colors.bg, colors.elevated, colors.soft],
            );
            let surface = super::code_theme::surface(appearance.code_theme, mode);
            Accordion::new("settings-code-preview")
                .bordered(false)
                .h_auto()
                .item(|item| {
                    item.open(open)
                        .title(
                            div()
                                .debug_selector(|| "settings-code-preview-title".into())
                                .text_size(px(12.))
                                .line_height(CONTROL_LINE_HEIGHT)
                                .child(t!("settings.code_preview")),
                        )
                        .child(
                            v_flex()
                                .debug_selector(|| "settings-code-preview-sample".into())
                                .w_full()
                                .p(px(12.))
                                .bg(surface)
                                .text_color(highlight.style.editor_foreground.unwrap_or(colors.ink))
                                .border_1()
                                .border_color(cx.theme().border)
                                .rounded(cx.theme().radius)
                                .font_family(super::ui_code_font(cx))
                                .text_size(px(12.))
                                .line_height(relative(1.5))
                                .children(preview_lines(&highlight)),
                        )
                })
                .on_toggle_click({
                    let page = page.clone();
                    move |items, _, cx| {
                        _ = page.update(cx, |page, cx| {
                            page.code_preview_open = items.contains(&0);
                            cx.notify();
                        });
                    }
                })
        })
        .keywords([t!("settings.code_preview"), Cow::Borrowed("preview")])
    }

    fn switch(&self, default: bool, field: fn(&mut Config) -> &mut bool) -> SettingField<bool> {
        let (read, write) = self.field(field);
        SettingField::switch(read, write).default_value(default)
    }

    fn size(&self, default: f64, field: fn(&mut Config) -> &mut f64) -> SettingField<f64> {
        self.number_in(default, FONT_SIZE_MIN..=FONT_SIZE_MAX, 0.5, field)
    }

    fn number_in(
        &self,
        default: f64,
        range: std::ops::RangeInclusive<f64>,
        step: f64,
        field: fn(&mut Config) -> &mut f64,
    ) -> SettingField<f64> {
        let (read, write) = self.field(field);
        SettingField::number_input(
            NumberFieldOptions {
                min: *range.start(),
                max: *range.end(),
                step,
            },
            read,
            write,
        )
        .default_value(default)
    }

    fn input(&self, field: fn(&mut Config) -> &mut String) -> SettingField<SharedString> {
        let (read, write) = self.field(field);
        SettingField::input(
            move |cx| read(cx).into(),
            move |value, cx| write(value.to_string(), cx),
        )
        .default_value(SharedString::default())
    }

    fn theme_picker(&self) -> SettingField<SharedString> {
        let (read, write) = self.field(|config| &mut config.appearance.theme);
        let write = Rc::new(write);
        let reset = write.clone();
        let dirty = self.draft.clone();
        SettingField::render(move |options, _, cx| {
            let current = read(cx);
            let write = write.clone();
            action_button("theme-picker")
                .map(|button| {
                    if options.layout().is_vertical() {
                        button.w_full()
                    } else {
                        button.min_w(px(128.))
                    }
                })
                .with_size(options.size())
                .disabled(options.is_disabled())
                .outline()
                .icon(current.icon())
                .icon_size(px(15.))
                .label(current.label())
                .label_size(options.font_size())
                .line_height(CONTROL_LINE_HEIGHT)
                .accessibility_label(
                    t!("settings.theme_label", theme = current.label()).into_owned(),
                )
                .dropdown_caret(true)
                .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                    Theme::MODES
                        .into_iter()
                        .fold(menu.check_side(Side::Right), |menu, mode| {
                            let write = write.clone();
                            menu.item(
                                PopupMenuItem::new(mode.label())
                                    .icon(mode.icon().with_size(px(15.)))
                                    .checked(mode == current)
                                    .on_click(move |_, _, cx| write(mode, cx)),
                            )
                        })
                })
        })
        .on_reset(
            move |cx| dirty.read(cx).appearance.theme != Theme::System,
            move |_, cx| reset(Theme::System, cx),
        )
    }

    fn font_picker(
        &self,
        id: &'static str,
        label: Cow<'static, str>,
        default_key: &'static str,
        field: fn(&mut Config) -> &mut String,
    ) -> SettingField<SharedString> {
        let (read, write) = self.field(field);
        let read = Rc::new(read);
        let write = Rc::new(write);
        let dirty = read.clone();
        let reset = write.clone();
        SettingField::render(move |options, window, cx| {
            let family: SharedString = read(cx).into();
            let picker = window.use_keyed_state(id, cx, |window, cx| {
                let mut fonts = vec![FontChoice {
                    family: SharedString::default(),
                    default_key,
                }];
                fonts.extend(cx.text_system().all_font_names().into_iter().map(|name| {
                    FontChoice {
                        family: name.into(),
                        default_key,
                    }
                }));
                let selected = fonts.iter().position(|font| font.family == family);
                let state = cx.new(|cx| {
                    SelectState::new(
                        FontChoices::from(fonts),
                        selected.map(IndexPath::new),
                        window,
                        cx,
                    )
                    .searchable(true)
                });
                let write = write.clone();
                let subscription =
                    cx.subscribe(&state, move |_, _, event: &SelectEvent<FontChoices>, cx| {
                        if let SelectEvent::Confirm(Some(family)) = event {
                            write(family.to_string(), cx);
                        }
                    });
                (state, subscription)
            });
            let state = picker.read(cx).0.clone();
            let selected = state.read(cx).selected_value();
            if selected != Some(&family) && (selected.is_some() || family.is_empty()) {
                state.update(cx, |state, cx| {
                    state.set_selected_value(&family, window, cx)
                });
            }
            Select::new(&state)
                .accessibility_label(label.clone())
                .placeholder(family)
                .search_placeholder(t!("settings.search_fonts"))
                .with_size(options.size())
                .text_size(options.font_size())
                .line_height(CONTROL_LINE_HEIGHT)
                .disabled(options.is_disabled())
                .map(|select| {
                    if options.layout().is_vertical() {
                        select.w_full()
                    } else {
                        select.w(px(215.))
                    }
                })
        })
        .on_reset(
            move |cx| !dirty(cx).is_empty(),
            move |_, cx| reset(String::new(), cx),
        )
    }

    fn dropdown(
        &self,
        options: &[(&'static str, Cow<'static, str>)],
        default: &'static str,
        read: fn(&Config) -> &'static str,
        write: fn(&mut Config, &str),
    ) -> SettingField<SharedString> {
        let draft = self.draft.clone();
        let change = self.draft.clone();
        SettingField::dropdown(
            options
                .iter()
                .map(|(value, label)| ((*value).into(), label.clone().into()))
                .collect(),
            move |cx| read(draft.read(cx)).into(),
            move |value, cx| {
                change.update(cx, |draft, cx| {
                    write(draft, &value);
                    cx.notify();
                })
            },
        )
        .default_value(default)
    }

    fn field<T: Clone + PartialEq + 'static>(
        &self,
        field: fn(&mut Config) -> &mut T,
    ) -> (impl Fn(&App) -> T + 'static, impl Fn(T, &mut App) + 'static) {
        let read = self.draft.clone();
        let write = self.draft.clone();
        (
            move |cx| field(&mut read.read(cx).clone()).clone(),
            move |value, cx| {
                write.update(cx, |draft, cx| {
                    let field = field(draft);
                    if *field != value {
                        *field = value;
                        cx.notify();
                    }
                })
            },
        )
    }
}

fn setting_item(
    title: impl Into<SharedString>,
    field: impl AnySettingField + 'static,
) -> SettingItem {
    SettingItem::new(title, field).reset_button(t!("settings.reset"))
}

fn preview_lines(highlight: &HighlightTheme) -> impl Iterator<Item = StyledText> + '_ {
    CODE_PREVIEW.iter().map(move |line| {
        let mut text = String::new();
        let mut highlights = Vec::new();
        for &(segment, capture) in *line {
            let start = text.len();
            text.push_str(segment);
            if let Some(style) = highlight.style(capture) {
                highlights.push((start..text.len(), style));
            }
        }
        StyledText::new(text).with_highlights(highlights)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::ConfigFile, repository::Store, workspace::WorkspaceView};
    use std::sync::Arc;

    #[::core::prelude::v1::test]
    fn ai_provider_settings_mask_the_key_and_fetch_model_ids() -> anyhow::Result<()> {
        use std::{
            io::{BufRead as _, BufReader, Write as _},
            net::TcpListener,
            time::Instant,
        };

        struct ProviderProbe(Entity<SettingsPage>);
        impl Render for ProviderProbe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                Settings::new("provider-test").page(SettingPage::new(t!("ai.page")).group(
                    SettingGroup::new().item(self.0.read(cx).provider_settings(self.0.downgrade())),
                ))
            }
        }

        crate::ai::install_test_keyring();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let base_url = format!("http://{}/v1", listener.local_addr()?);
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    assert!(reader.read_line(&mut request).unwrap() > 0);
                }
                assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("\r\nauthorization: bearer test-credential\r\n")
                );
                let body = r#"{"data":[{"id":"writer"},{"id":"reader"},{"id":"reader"}]}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .unwrap();
            }
        });
        let fixture = tempfile::tempdir()?;
        let config_path = fixture.path().join("config.toml");
        let config = ConfigFile::load(Some(config_path.clone()));
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
        let mut app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        app.update(gpui_component::init);
        let (owner, owner_window) = app.add_window_view(|window, cx| {
            Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
        });
        let owner_window = owner_window.update(|window, _| window.window_handle());
        let mut settings = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let draft = cx.new(|cx| owner.read(cx).config.current().clone());
            let page =
                cx.new(|cx| SettingsPage::new(owner.clone(), owner_window, draft, window, cx));
            settings = Some(page.clone());
            let probe = cx.new(|_| ProviderProbe(page));
            Root::new(probe, window, cx)
        });
        let page = settings.unwrap();
        visual.simulate_resize(size(px(760.), px(650.)));
        visual.update(|window, cx| {
            let settings = page.read(cx);
            let key = settings.ai_key.clone();
            assert!(key.read(cx).presentation().placeholder().is_empty());
            for (state, value) in [
                (settings.ai_base.clone(), base_url.as_str()),
                (key.clone(), "test-credential"),
            ] {
                state.update(cx, |input, cx| input.set_value(value, window, cx));
            }
            assert!(key.read(cx).presentation().is_masked());
            window.draw(cx).clear(cx);
        });
        let field_y = |visual: &mut VisualTestContext, selector| {
            visual.debug_bounds(selector).unwrap().origin.y
        };
        assert!(field_y(visual, "ai-base-url") < field_y(visual, "ai-api-key"));
        assert!(field_y(visual, "ai-api-key") < field_y(visual, "ai-model-id"));
        let click_and_wait = |visual: &mut VisualTestContext, selector| {
            let button = visual.debug_bounds(selector).unwrap();
            visual.simulate_click(button.center(), Modifiers::default());
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                visual.run_until_parked();
                if !visual.update(|_, cx| {
                    let settings = page.read(cx);
                    settings.ai_saving || settings.ai_fetching_models
                }) {
                    break;
                }
                assert!(Instant::now() < deadline, "{selector} did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
            visual.update(|window, cx| window.draw(cx).clear(cx));
        };
        click_and_wait(visual, "ai-fetch-models");
        let picker = visual
            .debug_bounds("ai-model-picker")
            .expect("model picker");
        assert_eq!(picker.size.height, px(26.));
        visual.update(|window, _| {
            let bounds = picker.scale(window.scale_factor());
            assert!(
                window
                    .painted_quads()
                    .iter()
                    .any(|quad| { quad.bounds == bounds && quad.border_widths.top.0 > 0. })
            );
        });
        visual.simulate_click(picker.center(), Modifiers::default());
        visual.simulate_keystrokes("down enter");
        visual.run_until_parked();
        assert_eq!(
            visual.update(|_, cx| page.read(cx).ai_model.read(cx).value()),
            "reader"
        );
        click_and_wait(visual, "ai-save-provider");
        let config = ConfigFile::load(Some(config_path.clone()));
        let saved = config.current().ai.clone();
        assert_eq!(saved.base_url, base_url);
        assert_eq!(saved.model, "reader");
        assert!(!saved.credential.is_empty());
        assert!(!std::fs::read_to_string(&config_path)?.contains("test-credential"));
        visual.update(|_, cx| {
            let key = page.read(cx).ai_key.clone();
            assert_eq!(key.read(cx).text().len(), 0);
            assert_eq!(
                key.read(cx).presentation().placeholder().as_str(),
                SAVED_API_KEY_MASK
            );
            assert!(owner.read(cx).ai.snapshot(true).is_some());
        });
        click_and_wait(visual, "ai-fetch-models");
        assert!(visual.debug_bounds("ai-model-picker").is_some());
        server.join().unwrap();
        visual.update(|window, cx| {
            let base = page.read(cx).ai_base.clone();
            base.update(cx, |base, cx| {
                base.replace_all("https://different.invalid", window, cx)
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let settings = page.read(cx);
            assert!(
                settings
                    .ai_key
                    .read(cx)
                    .presentation()
                    .placeholder()
                    .is_empty()
            );
            assert!(settings.ai_models.is_none());
            window.draw(cx).clear(cx);
        });
        click_and_wait(visual, "ai-fetch-models");
        assert!(visual.update(|_, cx| page.read(cx).ai_models_error.is_some()));
        click_and_wait(visual, "ai-save-provider");
        assert_eq!(
            ConfigFile::load(Some(config_path.clone())).current().ai,
            saved
        );
        assert!(visual.update(|_, cx| page.read(cx).error.is_some()));
        crate::ai::remove_credential(&config_path, &saved);
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn code_palette_preview_is_collapsed_until_opened() -> anyhow::Result<()> {
        let fixture = tempfile::tempdir()?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
        let mut app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        app.update(gpui_component::init);
        let (owner, owner_window) = app.add_window_view(|window, cx| {
            Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
        });
        let owner_window = owner_window.update(|window, _| window.window_handle());
        let mut settings_draft = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let draft = cx.new(|cx| owner.read(cx).config.current().clone());
            settings_draft = Some(draft.clone());
            let page = cx.new(|cx| SettingsPage::new(owner, owner_window, draft, window, cx));
            Root::new(page, window, cx)
        });
        let draft = settings_draft.unwrap();
        visual.simulate_resize(size(px(760.), px(760.)));
        let settle = |visual: &mut VisualTestContext| {
            for _ in 0..60 {
                visual.executor().advance_clock(Duration::from_millis(16));
                visual.update(|window, cx| {
                    window.simulate_next_frame(cx);
                    window.draw(cx).clear(cx);
                });
            }
        };
        settle(visual);
        assert!(
            visual
                .debug_bounds("settings-code-preview-sample")
                .is_none()
        );

        for open in [true, false] {
            let title = visual
                .debug_bounds("settings-code-preview-title")
                .expect("the preview title stays visible while collapsed");
            assert_eq!(title.size.height, px(18.));
            visual.simulate_click(title.center(), Modifiers::default());
            settle(visual);
            assert_eq!(
                visual
                    .debug_bounds("settings-code-preview-sample")
                    .is_some(),
                open
            );
        }

        visual.simulate_resize(size(px(560.), px(400.)));
        settle(visual);
        let title_at_default = visual.debug_bounds("settings-window-title").unwrap();
        let preview_title_size = visual
            .debug_bounds("settings-code-preview-title")
            .unwrap()
            .size;
        draft.update(visual, |draft, cx| {
            draft.appearance.ui_font_size = UI_FONT_SIZE_MAX;
            cx.notify();
        });
        settle(visual);
        let title_at_max = visual.debug_bounds("settings-window-title").unwrap();
        assert!(title_at_max.size.width > title_at_default.size.width);
        assert_eq!(title_at_max.size.height, title_at_default.size.height);
        assert_eq!(
            visual
                .debug_bounds("settings-code-preview-title")
                .unwrap()
                .size,
            preview_title_size
        );
        visual.update(|window, _| assert_eq!(window.rem_size(), px(UI_FONT_SIZE_MAX as f32)));

        Ok(())
    }
}
