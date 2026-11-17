#![cfg(target_os = "linux")]

use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

use gpui::{
    AppContext as _, Context, DevicePixels, Entity, HeadlessAppContext, Hsla, ParentElement as _,
    PlatformAtlas, PlatformHeadlessRenderer, Render, Scene, Size, Styled as _, Window, div, px,
    size,
};
use gpui_component::{
    ActiveTheme as _, Root, ThemeMode,
    diff::{Diff, DiffFile, DiffState},
    highlighter::SyntaxColors,
    input::{Editor, EditorState, Rope},
};
use gpui_wgpu::WgpuHeadlessRenderer;
use stroll::workspace::apply_theme;

const EDITOR_HEIGHT: f32 = 160.;
const DELIMITER: &str = "punctuation.delimiter.code";
const INLINE_CODE: &str = "text.code.span";
const SOURCE: &str =
    "Use `new` and ``a`b`` in prose.\n\n```bash\nif true; then echo new; fi\n```\n";
const PATCH: &str = concat!(
    "--- a/guide.markdown\n+++ b/guide.markdown\n@@ -1,5 +1,5 @@\n",
    "-Use `old` and ``a`b`` in prose.\n+Use `new` and ``a`b`` in prose.\n",
    " \n ```bash\n-if true; then echo old; fi\n+if true; then echo new; fi\n ```\n",
);

struct MarkdownSurfaces {
    editor: Entity<EditorState>,
    diff: Entity<DiffState>,
}

impl Render for MarkdownSurfaces {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .child(
                div()
                    .h(px(EDITOR_HEIGHT))
                    .flex_none()
                    .child(Editor::new(&self.editor).text_size(px(17.)).size_full()),
            )
            .child(
                div().flex_1().min_h_0().child(
                    Diff::new(&self.diff)
                        .line_number(false)
                        .code_font_size(px(17.))
                        .size_full(),
                ),
            )
    }
}

type GlyphColors = Rc<RefCell<Vec<(f32, Hsla)>>>;

struct Renderer {
    wgpu: WgpuHeadlessRenderer,
    glyphs: GlyphColors,
}

impl PlatformHeadlessRenderer for Renderer {
    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        viewport: Size<DevicePixels>,
    ) -> anyhow::Result<image::RgbaImage> {
        self.glyphs.replace(
            scene
                .monochrome_sprites
                .iter()
                .map(|sprite| (sprite.bounds.origin.y.0, sprite.color))
                .chain(
                    scene
                        .subpixel_sprites
                        .iter()
                        .map(|sprite| (sprite.bounds.origin.y.0, sprite.color)),
                )
                .collect(),
        );
        self.wgpu.render_scene_to_image(scene, viewport)
    }

    fn render_scene(&mut self, scene: &Scene, viewport: Size<DevicePixels>) -> anyhow::Result<()> {
        self.wgpu.render_scene(scene, viewport)
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.wgpu.sprite_atlas()
    }
}

fn color(syntax: &SyntaxColors, capture: &str) -> Hsla {
    syntax.style(capture).unwrap().color.unwrap()
}

#[test]
fn paints_markdown_code_in_editor_and_diff_in_both_themes() -> anyhow::Result<()> {
    let glyphs = GlyphColors::default();
    let mut app = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        {
            let glyphs = glyphs.clone();
            move || {
                Ok(Some(Box::new(Renderer {
                    wgpu: WgpuHeadlessRenderer::new()?,
                    glyphs: glyphs.clone(),
                })))
            }
        },
    );
    app.update(|cx| {
        gpui_component::init(cx);
        apply_theme(ThemeMode::Light, cx);
    });
    let window = app.open_window(size(px(820.), px(440.)), |window, cx| {
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("markdown")
                .line_number(false)
                .default_file_text(Rope::from(SOURCE));
            state.ensure_highlighter_factory(
                gpui_component::highlighter::input_highlighter_factory(),
            );
            state.prepare_language(window, cx);
            state
        });
        let diff = cx.new(|cx| DiffState::new(DiffFile::parse(PATCH).unwrap(), cx));
        let scene = cx.new(|_| MarkdownSurfaces { editor, diff });
        cx.new(|cx| Root::new(scene, window, cx))
    })?;
    let output = std::env::var_os("STROLL_THEME_SCREENSHOTS").map(PathBuf::from);

    for (mode, label) in [(ThemeMode::Light, "light"), (ThemeMode::Dark, "dark")] {
        app.update(|cx| apply_theme(mode, cx));
        for _ in 0..5 {
            app.run_until_parked();
            app.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
        }
        let screenshot = app.capture_screenshot(window.into())?;
        if let Some(output) = &output {
            screenshot.save(output.join(format!("markdown-code-{label}.png")))?;
        }
        let (syntax, removed, added, editor_bottom) =
            app.update_window(window.into(), |_, window, cx| {
                let style = &cx.theme().highlight_theme.style;
                (
                    style.syntax.clone(),
                    style.diff_removed.clone().unwrap(),
                    style.diff_added.clone().unwrap(),
                    EDITOR_HEIGHT * window.scale_factor(),
                )
            })?;
        let glyphs = glyphs.borrow();
        let count = |colors: &[Hsla], in_diff| {
            glyphs
                .iter()
                .filter(|(y, color)| (*y >= editor_bottom) == in_diff && colors.contains(color))
                .count()
        };
        for (capture, editor_count, diff_count) in
            [(DELIMITER, 12, 18), (INLINE_CODE, 6, 12), ("keyword", 2, 4)]
        {
            let editor_color = color(&syntax, capture);
            assert_ne!(
                editor_color,
                color(&syntax, "primary"),
                "{label}: {capture}"
            );
            assert!(
                count(&[editor_color], false) >= editor_count,
                "{label}: editor {capture}"
            );
            let diff_colors = [
                editor_color,
                color(&removed.0.syntax, capture),
                color(&removed.1.syntax, capture),
                color(&added.0.syntax, capture),
                color(&added.1.syntax, capture),
            ];
            let actual = count(&diff_colors, true);
            assert!(
                actual >= diff_count,
                "{label}: diff {capture}, expected at least {diff_count}, got {actual}"
            );
        }
    }
    Ok(())
}
