use rust_i18n::t;

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use gpui::{
    App, Image, ImageFormat, IntoElement, ObjectFit, ParentElement as _, SharedString, Styled,
    StyledImage as _, Window, div, img, px,
};
use gpui_component::{
    ActiveTheme, ThemeMode, h_flex,
    scroll::ScrollableElement as _,
    text::{
        InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
        MarkdownPlugin, markdown_ast::Node,
    },
};
use merman::{
    OperationControl, RenderOutput, RenderRequest, Renderer, SvgEnvironment, SvgRequest,
    svg::{
        HostTheme, HostThemePreset, Presentation, PresentationProfile, RenderResourcePolicy,
        SvgPipeline,
    },
};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_svg::{SvgColorSyntax, SvgOptions, render_to_svg_with_color_syntax};
use ratex_types::{color::Color, math_style::MathStyle};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum GraphicKind {
    Diagram,
    DisplayMath,
    InlineMath,
}

impl GraphicKind {
    pub fn source(self, node: &Node) -> Option<&str> {
        match (self, node) {
            (Self::Diagram, Node::Code(code)) if code.lang.as_deref() == Some("mermaid") => {
                Some(&code.value)
            }
            (Self::DisplayMath, Node::Math(math)) => Some(&math.value),
            (Self::InlineMath, Node::InlineMath(math)) => Some(&math.value),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Diagram => "mermaid",
            Self::DisplayMath => "display-math",
            Self::InlineMath => "inline-math",
        }
    }
}

#[derive(Clone)]
struct Picture {
    image: Arc<Image>,
    width: f32,
    height: f32,
    baseline: f32,
}

type ThemedPictures = [Result<Picture, SharedString>; 2];

#[derive(Default)]
pub struct Graphics(HashMap<GraphicKind, HashMap<String, ThemedPictures>>);

impl Graphics {
    pub fn prepare(&mut self, kind: GraphicKind, source: &str) {
        // Bound work per document and deduplicate repeated formulas.
        if self.0.values().map(HashMap::len).sum::<usize>() >= 128 {
            return;
        }
        self.0
            .entry(kind)
            .or_default()
            .entry(source.to_owned())
            .or_insert_with(|| {
                [false, true].map(|dark| {
                    let picture = match kind {
                        GraphicKind::Diagram => diagram(source, dark),
                        _ => formula(source, dark, kind == GraphicKind::InlineMath),
                    };
                    picture.map_err(|error| format!("{error:#}").into())
                })
            });
    }

    fn get(
        &self,
        kind: GraphicKind,
        source: &str,
        cx: &App,
    ) -> Option<&Result<Picture, SharedString>> {
        let theme = usize::from(cx.theme().mode == ThemeMode::Dark);
        self.0.get(&kind)?.get(source).map(|images| &images[theme])
    }
}

pub fn extensions(graphics: Arc<Graphics>) -> MarkdownExtensions {
    [
        GraphicKind::Diagram,
        GraphicKind::DisplayMath,
        GraphicKind::InlineMath,
    ]
    .into_iter()
    .fold(
        MarkdownExtensions::default().frontmatter(),
        |extensions, kind| {
            extensions.plugin(GraphicPlugin {
                kind,
                graphics: graphics.clone(),
            })
        },
    )
}

struct GraphicPlugin {
    kind: GraphicKind,
    graphics: Arc<Graphics>,
}

impl MarkdownPlugin for GraphicPlugin {
    fn is_block(&self) -> bool {
        self.kind != GraphicKind::InlineMath
    }

    fn name(&self) -> &str {
        self.kind.name()
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let source = self.kind.source(node)?;
        Some(
            MarkdownNode::new(self.name().to_owned(), source.to_owned())
                .text(source.to_owned())
                .markdown(cx.node_source(node).unwrap_or(source).to_owned()),
        )
    }

    fn render(&self, node: &MarkdownNode, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let source = node.as_text();
        let theme = cx.theme();
        let inner = match self.graphics.get(self.kind, source, cx) {
            Some(Ok(picture)) => div()
                .w_full()
                .min_w_0()
                .h(px(picture.height + 8.))
                // Document graphics are content: keep a standard-width
                // scrollbar visible rather than fading it out.
                .overflow_x_content_scrollbar()
                .id((
                    "document-graphic",
                    node.source_range().map_or(0, |range| range.start),
                ))
                .child(
                    img(picture.image.clone())
                        .flex_shrink_0()
                        .w(px(picture.width))
                        .h(px(picture.height))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            result => {
                let error = result
                    .and_then(|result| result.as_ref().err())
                    .cloned()
                    .unwrap_or_else(|| t!("docs.graphics_limit").into());
                div()
                    .w_full()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(t!("docs.render_failed")),
                    )
                    .child(div().mt_1().child(error))
                    .child(
                        div()
                            .mt_2()
                            .font_family(crate::workspace::ui_code_font(cx))
                            .child(source.to_owned()),
                    )
                    .into_any_element()
            }
        };
        let container = h_flex().w_full().min_w_0().justify_center();
        if self.kind == GraphicKind::DisplayMath {
            // Display math is typography, not a figure: no panel chrome, the
            // formula sits directly on the reading surface.
            container.child(inner)
        } else {
            container
                .p_3()
                .rounded(px(10.))
                .border_1()
                .border_color(theme.border)
                .bg(theme.muted)
                .child(inner)
        }
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let picture = self
            .graphics
            .get(self.kind, node.as_text(), cx)?
            .as_ref()
            .ok()?;
        let scale = f32::from(context.font_size()) / 16.;
        Some(
            InlineElement::new(
                img(picture.image.clone())
                    .w(px(picture.width * scale))
                    .h(px(picture.height * scale))
                    .object_fit(ObjectFit::Contain),
            )
            .with_baseline(px(picture.baseline * scale)),
        )
    }
}

fn formula(source: &str, dark: bool, inline: bool) -> Result<Picture> {
    ensure!(source.len() <= 8192, t!("docs.formula_too_long"));
    let nodes = ratex_parser::parse(source).map_err(|error| anyhow::anyhow!("{error}"))?;
    let color = if dark {
        Color::new(0.86, 0.87, 0.89, 1.)
    } else {
        Color::new(0.14, 0.15, 0.17, 1.)
    };
    let style = if inline {
        MathStyle::Text
    } else {
        MathStyle::Display
    };
    let list = to_display_list(&layout(
        &nodes,
        &LayoutOptions::default().with_style(style).with_color(color),
    ));
    let font_size = if inline { 16. } else { 19. };
    let padding = if inline { 1. } else { 4. };
    let svg = render_to_svg_with_color_syntax(
        &list,
        &SvgOptions {
            font_size: font_size * 2.,
            padding: padding * 2.,
            embed_glyphs: true,
            ..SvgOptions::default()
        },
        SvgColorSyntax::Rgb,
    );
    let (width, height) = svg_size(&svg)?;
    Ok(Picture {
        image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
        width: width / 2.,
        height: height / 2.,
        baseline: (list.height * font_size + padding) as f32,
    })
}

fn diagram(source: &str, dark: bool) -> Result<Picture> {
    let theme = if dark {
        HostThemePreset::EditorDark
    } else {
        HostThemePreset::EditorLight
    };
    let presentation = Presentation::new()
        .with_profile(PresentationProfile::MermanModern)
        .with_theme(HostTheme::from_preset(theme))
        .resolve();
    let renderer =
        Renderer::new().with_engine(presentation.materialize_engine(merman::Engine::new()));
    let request = SvgRequest {
        environment: SvgEnvironment::deterministic()
            .with_resource_policy(RenderResourcePolicy::interactive()),
        pipeline: Some(SvgPipeline::resvg_safe()),
        presentation: presentation.render_policy(),
        ..SvgRequest::default()
    };
    let output = renderer.render(RenderRequest::svg(
        source,
        OperationControl::new().with_deadline(Duration::from_secs(2)),
        request,
    ))?;
    let RenderOutput::Svg(Some(output)) = output else {
        anyhow::bail!(t!("docs.no_mermaid"));
    };
    let (svg, _) = output.into_parts();
    let (width, height) = svg_size(&svg)?;
    let scale = (440. / height).min(1.);
    Ok(Picture {
        image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
        width: width * scale,
        height: height * scale,
        baseline: 0.,
    })
}

fn svg_size(svg: &str) -> Result<(f32, f32)> {
    let xml = roxmltree::Document::parse(svg)?;
    let root = xml.root_element();
    let parse = |value: &str| value.trim_end_matches("px").parse::<f32>().ok();
    let dimensions = root
        .attribute("viewBox")
        .and_then(|value| {
            let mut values = value
                .split(|ch: char| ch.is_ascii_whitespace() || ch == ',')
                .filter(|part| !part.is_empty())
                .skip(2);
            Some((parse(values.next()?)?, parse(values.next()?)?))
        })
        .or_else(|| {
            Some((
                parse(root.attribute("width")?)?,
                parse(root.attribute("height")?)?,
            ))
        });
    let (width, height) = dimensions.context(t!("docs.svg_invalid_size"))?;
    ensure!(
        width.is_finite()
            && height.is_finite()
            && width > 0.
            && height > 0.
            && width <= 8192.
            && height <= 8192.
            && width * height <= 16_000_000.,
        t!("docs.svg_size_limit")
    );
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_mermaid_fixtures_produce_rasterizable_images() {
        let rasterizer = gpui::SvgRenderer::new(Arc::new(()));
        let source = include_str!("../tests/fixtures/mermaid-layout.md").replace("\r\n", "\n");
        let mut count = 0;
        for block in source.split("```mermaid\n").skip(1) {
            let (source, _) = block.split_once("\n```").unwrap();
            let image = diagram(source, false).unwrap().image;
            image.to_image_data(rasterizer.clone()).unwrap();
            count += 1;
        }
        assert_eq!(count, 7);
    }

    #[test]
    fn inline_and_display_math_produce_svg_with_expected_baseline() {
        let rasterizer = gpui::SvgRenderer::new(Arc::new(()));
        for (source, inline) in [(r"E=mc^2", true), (r"\frac{1}{\sqrt{2\pi}}", false)] {
            let picture = formula(source, true, inline).unwrap();
            assert!(picture.baseline > 0. && picture.baseline < picture.height);
            picture.image.to_image_data(rasterizer.clone()).unwrap();
        }
    }
}
