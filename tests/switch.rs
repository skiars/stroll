#![cfg(target_os = "linux")]

use std::{fs, path::PathBuf, sync::Arc};

use gpui::{
    AppContext as _, Background, Context, HeadlessAppContext, Hsla, IntoElement,
    ParentElement as _, Render, Styled as _, Window, div, px, size,
};
use gpui_component::{ActiveTheme as _, ThemeMode, switch::Switch};
use stroll::workspace::apply_theme;

struct SwitchPreview;

impl Render for SwitchPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .flex()
            .items_center()
            .justify_center()
            .gap(px(24.))
            .child(Switch::new("off").label("Off"))
            .child(Switch::new("on").label("On").checked(true))
    }
}

#[test]
fn shared_switch_renders_compact_geometry_in_both_themes() -> anyhow::Result<()> {
    let screenshots = std::env::var_os("STROLL_SWITCH_SCREENSHOTS").map(PathBuf::from);
    let capture = screenshots.is_some();
    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        move || {
            if capture {
                Ok(Some(Box::new(gpui_wgpu::WgpuHeadlessRenderer::new()?)))
            } else {
                Ok(None)
            }
        },
    );
    cx.update(gpui_component::init);
    let handle = cx.open_window(size(px(250.), px(72.)), |_, cx| cx.new(|_| SwitchPreview))?;
    cx.run_until_parked();

    for (mode, name) in [(ThemeMode::Light, "light"), (ThemeMode::Dark, "dark")] {
        cx.update_window(handle.into(), |_, window, cx| {
            apply_theme(mode, cx);
            window.refresh();
            window.draw(cx).clear(cx);

            let scale = window.scale_factor();
            let track_size = size(px(34.).scale(scale), px(20.).scale(scale));
            let thumb_size = size(px(16.).scale(scale), px(16.).scale(scale));
            let inset = px(2.).scale(scale);
            let quads = window.painted_quads();
            for (checked, track_color) in [
                (false, Background::from(cx.theme().tokens.switch)),
                (true, Background::from(cx.theme().tokens.primary)),
            ] {
                let track = quads
                    .iter()
                    .find(|quad| quad.bounds.size == track_size && quad.background == track_color)
                    .expect("the track has the expected size and theme color");
                assert!(track.corner_radii.top_left >= px(10.).scale(scale));
                let thumb_left = if checked {
                    track.bounds.right() - thumb_size.width - inset
                } else {
                    track.bounds.left() + inset
                };
                assert!(quads.iter().any(|quad| {
                    quad.bounds.size == thumb_size
                        && quad.bounds.left() == thumb_left
                        && quad.bounds.top() == track.bounds.top() + inset
                        && quad.background == Background::from(Hsla::white())
                        && quad.corner_radii.top_left >= px(8.).scale(scale)
                }));
            }
        })?;
        if let Some(output) = &screenshots {
            fs::create_dir_all(output)?;
            cx.capture_screenshot(handle.into())?
                .save(output.join(format!("switch-{name}.png")))?;
        }
    }
    Ok(())
}
