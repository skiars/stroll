#![cfg(target_os = "linux")]

use std::sync::Arc;

use gpui::{
    AppContext as _, Context, Entity, Focusable as _, HeadlessAppContext, IntoElement, Render,
    Styled as _, Window, px, size,
};
use gpui_component::{
    Root, ThemeMode,
    input::{Editor, EditorState, InlayHint},
};
use gpui_wgpu::WgpuHeadlessRenderer;

struct SourceEditor(Entity<EditorState>);

impl Render for SourceEditor {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Editor::new(&self.0).text_size(px(21.)).size_full()
    }
}

#[test]
fn vim_block_caret_uses_shaped_cells_and_inverts_the_glyph() -> anyhow::Result<()> {
    let mut app = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
    );
    app.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });

    for (theme, background, light_text) in [
        (ThemeMode::Light, [0x58, 0x5a, 0x61], true),
        (ThemeMode::Dark, [0xd4, 0xd6, 0xda], false),
    ] {
        app.update(|cx| stroll::workspace::apply_theme(theme, cx));
        let mut editor = None;
        let window = app
            .open_window(size(px(430.), px(260.)), |window, cx| {
                let state = cx.new(|cx| EditorState::new(window, cx).default_value("M \n\n"));
                editor = Some(state.clone());
                let view = cx.new(|_| SourceEditor(state));
                cx.new(|cx| Root::new(view, window, cx))
            })?
            .into();
        let editor = editor.unwrap();
        app.update_window(window, |_, window, _| window.activate_window())?;
        app.run_until_parked();
        let thin = app.update_window(window, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.focus(window, cx);
                editor.set_inlay_hints(
                    [(0, "type: "), (1, ": value"), (2, " ending")]
                        .map(|(offset, label)| InlayHint {
                            offset,
                            label: label.into(),
                        })
                        .to_vec(),
                    cx,
                );
            });
            window.draw(cx).clear(cx);
            editor.read(cx).cursor_layout().unwrap().0
        })?;
        let mut inspect = |offset| -> anyhow::Result<_> {
            app.update_window(window, |_, window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.set_modal_editing(true, cx);
                    editor.set_modal_selection(&[(offset, offset)], offset, cx);
                    editor.focus(window, cx);
                });
                window.draw(cx).clear(cx);
            })?;
            app.run_until_parked();
            let (rect, scale) = app.update_window(window, |_, window, cx| {
                window.draw(cx).clear(cx);
                assert!(
                    window.is_window_active()
                        && editor.read(cx).focus_handle(cx).is_focused(window)
                );
                (
                    editor.read(cx).cursor_layout().unwrap().0,
                    window.scale_factor(),
                )
            })?;
            let screenshot = app.capture_screenshot(window)?;
            let mut solid = 0;
            let mut glyph = 0;
            for y in (rect.top().as_f32() * scale).ceil() as u32
                ..(rect.bottom().as_f32() * scale).floor() as u32
            {
                for x in (rect.left().as_f32() * scale).ceil() as u32
                    ..(rect.right().as_f32() * scale).floor() as u32
                {
                    let pixel = screenshot.get_pixel(x, y);
                    if pixel.0[..3] == background && pixel.0[3] == 255 {
                        solid += 1;
                    }
                    if pixel.0[..3].iter().all(|&channel| {
                        if light_text {
                            channel > 220
                        } else {
                            channel < 35
                        }
                    }) {
                        glyph += 1;
                    }
                }
            }
            assert!(
                solid > 20,
                "expected an opaque neutral block at byte {offset}"
            );
            assert_eq!(
                rect.size.height, thin.size.height,
                "block and text carets share a height"
            );
            Ok((rect, glyph))
        };
        let (letter, glyph) = inspect(0)?;
        assert!(
            glyph > 10,
            "expected contrasting glyph pixels inside the block"
        );
        let (space, blank) = inspect(1)?;
        assert_eq!(blank, 0, "a space has no glyph");
        let mut previous_top = space.top();
        for offset in [2, 3, 4] {
            let (bounds, glyph) = inspect(offset)?;
            assert!(
                (bounds.size.width - space.size.width).abs() < px(0.01),
                "line tail and empty line use a shaped space"
            );
            assert_eq!(glyph, 0);
            if offset > 2 {
                assert!(
                    bounds.top() > previous_top,
                    "empty lines have distinct rows"
                );
            }
            previous_top = bounds.top();
        }
        assert!(letter.size.width > px(2.));
        assert_eq!(
            letter.left(),
            thin.left(),
            "the block starts on the same source glyph as the text caret"
        );
        assert!(
            space.left() > letter.right() + letter.size.width,
            "the block skips the middle hint"
        );
    }
    Ok(())
}
