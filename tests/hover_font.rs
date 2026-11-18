#![cfg(any(target_os = "linux", target_os = "windows"))]

//! The LSP popovers are rendered inside the editor, so the code they show has
//! to be shaped in the editor's font. Prose keeps the UI font.

use std::{
    borrow::Cow,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui::{
    AppContext as _, Bounds, Context, DevicePixels, Font, FontId, FontMetrics, FontRun, FontWeight,
    GlyphId, IntoElement, LineLayout, ParentElement as _, Pixels, PlatformTextSystem, Render,
    RenderGlyphParams, ShapedGlyph, ShapedRun, Size, Styled as _, Task, TestAppContext,
    TestDispatcher, TextRenderingMode, Window, div, point, px, size,
};
use gpui_component::{
    Root, ThemeMode,
    input::{Editor, EditorState, HoverDocumentation, HoverParagraph, HoverProvider, Rope},
};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

const UI_FONT: &str = ".SystemUIFont";
const CODE_FONT: &str = "Editor Mono";
/// Only the hover documentation spells the symbol this way, so what is shaped
/// with it can only come from the popover.
const SYMBOL: &str = "hover_doc_symbol";
const SIGNATURE: &str = "```rust\nfn hover_doc_symbol()\n```\n\n";
const ORIGINAL: &str = "Returns `true` if the option is a [`None`](https://example.com) value.";
const TRANSLATION: &str =
    "若选项为 [`None`](https://example.com)，则返回 `true`，否则返回 `false`。";

struct Documentation;

impl HoverProvider for Documentation {
    fn hover(
        &self,
        _: &Rope,
        _: usize,
        _: &mut Window,
        _: &mut gpui::App,
    ) -> Task<anyhow::Result<Option<Hover>>> {
        Task::ready(Ok(Some(Hover {
            range: None,
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("{SIGNATURE}{ORIGINAL}"),
            }),
        })))
    }

    fn enrich_hover(
        &self,
        _: &Hover,
        _: usize,
        _: &mut Window,
        cx: &mut gpui::App,
    ) -> Task<Option<HoverDocumentation>> {
        let timer = cx.background_executor().timer(Duration::from_millis(450));
        cx.background_spawn(async move {
            timer.await;
            Some(HoverDocumentation {
                paragraphs: Arc::new(std::collections::BTreeMap::from([(
                    SIGNATURE.len(),
                    HoverParagraph {
                        original: ORIGINAL.into(),
                        translation: TRANSLATION.into(),
                    },
                )])),
                plaintext: None,
            })
        })
    }
}

/// Font, byte offset and byte length of one shaped run.
type RunSpan = (FontId, usize, usize);
type ShapedLine = (String, Pixels, Vec<RunSpan>);

/// A text system that hands every font its own id and records which
/// run every shaped line used.
#[derive(Default)]
struct RecordingTextSystem {
    fonts: Mutex<Vec<(FontId, Font)>>,
    shaped: Mutex<Vec<ShapedLine>>,
}

impl RecordingTextSystem {
    fn record_font(&self, font: &Font) -> FontId {
        let mut fonts = self.fonts.lock().unwrap();
        if let Some((id, _)) = fonts.iter().find(|(_, known)| known == font) {
            return *id;
        }
        let id = FontId(fonts.len() + 1);
        fonts.push((id, font.clone()));
        id
    }

    fn font(&self, id: FontId) -> Option<Font> {
        self.fonts
            .lock()
            .unwrap()
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, font)| font.clone())
    }

    /// The font and size of every run that covers `needle`.
    fn fonts_covering(&self, needle: &str) -> Vec<(Font, Pixels)> {
        let shaped = self.shaped.lock().unwrap();
        let mut fonts = Vec::new();
        for (text, size, runs) in shaped.iter() {
            let Some(start) = text.find(needle) else {
                continue;
            };
            let end = start + needle.len();
            for (font_id, run_start, run_len) in runs {
                if *run_start < end && start < run_start + run_len {
                    fonts.extend(self.font(*font_id).map(|font| (font, *size)));
                }
            }
        }
        fonts
    }
}

impl PlatformTextSystem for RecordingTextSystem {
    fn add_fonts(&self, _: Vec<Cow<'static, [u8]>>) -> anyhow::Result<()> {
        Ok(())
    }

    fn all_font_names(&self) -> Vec<String> {
        vec![CODE_FONT.to_owned(), UI_FONT.to_owned()]
    }

    fn font_id(&self, descriptor: &Font) -> anyhow::Result<FontId> {
        Ok(self.record_font(descriptor))
    }

    fn font_metrics(&self, _: FontId) -> FontMetrics {
        FontMetrics {
            units_per_em: 1000,
            ascent: 800.,
            descent: -200.,
            line_gap: 0.,
            underline_position: -100.,
            underline_thickness: 50.,
            cap_height: 700.,
            x_height: 500.,
            bounding_box: Bounds {
                origin: point(0., -200.),
                size: size(1000., 1000.),
            },
        }
    }

    fn typographic_bounds(&self, _: FontId, _: GlyphId) -> anyhow::Result<Bounds<f32>> {
        Ok(Bounds {
            origin: point(0., 0.),
            size: size(500., 700.),
        })
    }

    fn advance(&self, _: FontId, _: GlyphId) -> anyhow::Result<Size<f32>> {
        Ok(size(500., 0.))
    }

    fn glyph_for_char(&self, _: FontId, ch: char) -> Option<GlyphId> {
        Some(GlyphId(u32::from(ch)))
    }

    fn glyph_raster_bounds(&self, _: &RenderGlyphParams) -> anyhow::Result<Bounds<DevicePixels>> {
        Ok(Bounds::default())
    }

    fn rasterize_glyph(
        &self,
        _: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> anyhow::Result<(Size<DevicePixels>, Vec<u8>)> {
        Ok((raster_bounds.size, Vec::new()))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let mut position = px(0.);
        let mut shaped_runs = Vec::new();
        let mut recorded_runs = Vec::new();
        let mut run_start = 0;
        for run in runs {
            let run_text = &text[run_start..run_start + run.len];
            let advance = font_size * 0.5;
            let mut glyphs = Vec::new();
            for (ix, ch) in run_text.char_indices() {
                glyphs.push(ShapedGlyph {
                    id: GlyphId(u32::from(ch)),
                    position: point(position, px(0.)),
                    index: run_start + ix,
                    is_emoji: false,
                });
                position += advance;
            }
            shaped_runs.push(ShapedRun {
                font_id: run.font_id,
                glyphs,
            });
            recorded_runs.push((run.font_id, run_start, run.len));
            run_start += run.len;
        }
        self.shaped
            .lock()
            .unwrap()
            .push((text.to_owned(), font_size, recorded_runs));

        let metrics = self.font_metrics(FontId(0));
        LineLayout {
            font_size,
            width: position,
            ascent: font_size * (metrics.ascent / metrics.units_per_em as f32),
            descent: font_size * (metrics.descent / metrics.units_per_em as f32),
            runs: shaped_runs,
            len: text.len(),
        }
    }

    fn recommended_rendering_mode(&self, _: FontId, _: Pixels) -> TextRenderingMode {
        TextRenderingMode::Grayscale
    }
}

struct EditorProbe(gpui::Entity<EditorState>);

impl Render for EditorProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(Editor::new(&self.0).font_family(CODE_FONT).size_full())
    }
}

#[test]
fn hover_markdown_shapes_code_in_the_editor_font() {
    let text_system = Arc::new(RecordingTextSystem::default());
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        Some("hover_markdown_shapes_code_in_the_editor_font"),
        text_system.clone(),
    );
    cx.update(|cx| {
        gpui_component::init(cx);
        stroll::workspace::apply_theme(ThemeMode::Light, cx);
        cx.set_reduce_motion(true);
    });

    let mut editor = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let text = "fn source_line() {}\n".repeat(8);
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).default_value(text);
            state.lsp_mut().hover_provider = Some(Rc::new(Documentation));
            state
        });
        editor = Some(state.clone());
        let probe = cx.new(|_| EditorProbe(state));
        Root::new(probe, window, cx)
    });
    let editor = editor.unwrap();
    visual.simulate_resize(size(px(600.), px(400.)));
    visual.update(|window, cx| window.draw(cx).clear(cx));

    let anchor = visual.update(|_, cx| editor.read(cx).range_to_bounds(&(3..9)).unwrap().center());
    visual.simulate_mouse_move(anchor, None, Default::default());

    for (delay, enriched) in [(500, false), (500, true)] {
        visual.run_until_parked();
        visual
            .executor()
            .advance_clock(Duration::from_millis(delay));
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            visual.update(|_, cx| editor
                .read(cx)
                .hover_popover()
                .unwrap()
                .documentation
                .is_some()),
            enriched
        );
        if enriched {
            assert!(visual.debug_bounds("hover-original-paragraph").is_some());
            assert!(visual.debug_bounds("hover-translated-paragraph").is_some());
        }
        for needle in [SYMBOL, "true", "None", "Returns"].into_iter().chain(
            enriched
                .then_some(["false", "若选项为"])
                .into_iter()
                .flatten(),
        ) {
            let expected = match needle {
                "Returns" | "若选项为" => (UI_FONT, px(12.5)),
                SYMBOL => (CODE_FONT, px(12.5)),
                _ => (CODE_FONT, px(12.)),
            };
            let fonts = text_system.fonts_covering(needle);
            assert!(
                !fonts.is_empty()
                    && fonts
                        .iter()
                        .all(|(font, size)| (font.family.as_ref(), *size) == expected),
                "{needle} should use {expected:?} (enriched: {enriched}), got {fonts:?}"
            );
            if matches!(needle, "Returns" | "若选项为") {
                assert!(
                    fonts
                        .iter()
                        .all(|(font, _)| font.weight == FontWeight::NORMAL),
                    "{needle} should use the default prose weight, got {fonts:?}"
                );
            }
        }
    }
}
