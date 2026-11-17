#![cfg(any(target_os = "linux", target_os = "windows"))]

//! The LSP popovers are rendered inside the editor, so the code they show has
//! to be shaped in the editor's font. Prose keeps the UI font.

use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

use gpui::{
    AppContext as _, Bounds, Context, DevicePixels, Font, FontId, FontMetrics, FontRun, GlyphId,
    IntoElement, LineLayout, ParentElement as _, Pixels, PlatformTextSystem, Render,
    RenderGlyphParams, ShapedGlyph, ShapedRun, SharedString, Size, Styled as _, TestAppContext,
    TestDispatcher, TextRenderingMode, Window, div, point, px, size,
};
use gpui_component::{
    Root,
    input::{Editor, EditorState},
};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

const UI_FONT: &str = ".SystemUIFont";
const CODE_FONT: &str = "Editor Mono";
/// Only the hover documentation spells the symbol this way, so what is shaped
/// with it can only come from the popover.
const SYMBOL: &str = "hover_doc_symbol";
const HOVER: &str =
    "```rust\nfn hover_doc_symbol()\n```\n\nDocumentation for `hover_doc_symbol`.\n";

/// Font, byte offset and byte length of one shaped run.
type RunSpan = (FontId, usize, usize);

/// A text system that hands every family its own font id and records which
/// run every shaped line used.
#[derive(Default)]
struct RecordingTextSystem {
    families: Mutex<Vec<(FontId, SharedString)>>,
    /// `(text, runs)` per shaped line.
    shaped: Mutex<Vec<(String, Vec<RunSpan>)>>,
}

impl RecordingTextSystem {
    fn family_id(&self, family: SharedString) -> FontId {
        let mut families = self.families.lock().unwrap();
        if let Some((id, _)) = families.iter().find(|(_, known)| *known == family) {
            return *id;
        }
        let id = FontId(families.len() + 1);
        families.push((id, family));
        id
    }

    fn family(&self, id: FontId) -> Option<SharedString> {
        self.families
            .lock()
            .unwrap()
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, family)| family.clone())
    }

    /// The shaped lines that contain `needle`.
    fn lines_containing(&self, needle: &str) -> Vec<String> {
        self.shaped
            .lock()
            .unwrap()
            .iter()
            .map(|(text, _)| text.clone())
            .filter(|text| text.contains(needle))
            .collect()
    }

    /// The families of every run that covers `needle` in a shaped line.
    fn families_covering(&self, needle: &str) -> Vec<SharedString> {
        let shaped = self.shaped.lock().unwrap();
        let mut families = Vec::new();
        for (text, runs) in shaped.iter() {
            let Some(start) = text.find(needle) else {
                continue;
            };
            let end = start + needle.len();
            for (font_id, run_start, run_len) in runs {
                if *run_start < end && start < run_start + run_len {
                    families.extend(self.family(*font_id));
                }
            }
        }
        families
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
        Ok(self.family_id(descriptor.family.clone()))
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
            .push((text.to_owned(), recorded_runs));

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
        cx.set_reduce_motion(true);
    });

    let mut editor = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let text = "fn source_line() {}\n".repeat(8);
        let state = cx.new(|cx| EditorState::new(window, cx).default_value(text));
        editor = Some(state.clone());
        let probe = cx.new(|_| EditorProbe(state));
        Root::new(probe, window, cx)
    });
    let editor = editor.unwrap();
    visual.simulate_resize(size(px(600.), px(400.)));
    visual.update(|window, cx| window.draw(cx).clear(cx));

    visual.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            editor.present_hover(
                3..9,
                Hover {
                    range: None,
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: HOVER.to_owned(),
                    }),
                },
                cx,
            );
        });
    });
    visual.update(|window, cx| window.draw(cx).clear(cx));

    assert!(visual.update(|_, cx| editor.read(cx).hover_popover().is_some()));
    let lines = text_system.lines_containing(SYMBOL);
    assert!(
        lines.iter().any(|line| line.trim() == SYMBOL),
        "the inline code span must be shaped: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("fn ") && line.contains(SYMBOL)),
        "the fenced block must be shaped: {lines:?}"
    );
    let families = text_system.families_covering(SYMBOL);
    assert!(
        !families.is_empty(),
        "the hover documentation must be shaped at all"
    );
    assert!(
        families.iter().all(|family| family.as_ref() == CODE_FONT),
        "hover code must be shaped in the editor font, got {families:?}"
    );
}
