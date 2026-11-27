mod mermaid;

use rust_i18n::t;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Image, ImageContent, ImageFormat, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, ParsedSvg, SharedString, Styled, StyledImage as _, WeakEntity, Window,
    canvas, div, img, px,
};
use gpui_component::{
    ActiveTheme, ThemeMode, h_flex,
    scroll::ScrollableElement as _,
    text::{
        InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
        MarkdownPlugin, markdown_ast::Node,
    },
};
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_svg::{SvgColorSyntax, SvgOptions, render_to_svg_with_color_syntax};
use ratex_types::{color::Color, math_style::MathStyle};

use crate::docs::DocReader;

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
struct Picture<T = Arc<Image>> {
    image: T,
    width: f32,
    height: f32,
    baseline: f32,
    reflowable: bool,
}

const MATH_FONT_SIZE: f32 = 16.;

impl Picture<Arc<ParsedSvg>> {
    fn element(&self, scale: f32) -> gpui::Img {
        img(ImageContent::svg(self.image.clone()))
            .w(px(self.width * scale))
            .h(px(self.height * scale))
            .object_fit(ObjectFit::Fill)
    }
}

#[derive(Clone)]
enum GraphicState {
    Pending,
    Ready(Result<Picture<Arc<ParsedSvg>>, SharedString>),
    LimitReached,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct GraphicRequest {
    kind: GraphicKind,
    source: SharedString,
    dark: bool,
    reflowed: bool,
    diagram_fonts: Option<mermaid::DiagramFonts>,
}

#[derive(Clone)]
struct DiagramPresentation {
    picture: Picture<Arc<ParsedSvg>>,
    scale: f32,
    width: f32,
    dark: bool,
    fonts: Option<mermaid::DiagramFonts>,
}

#[derive(Default)]
struct GeneratedGraphics {
    images: HashMap<GraphicRequest, GraphicState>,
    diagram_widths: HashMap<usize, f32>,
    diagrams: HashMap<usize, DiagramPresentation>,
    pending: usize,
    reader: Option<WeakEntity<DocReader>>,
}

#[derive(Default)]
pub struct Graphics {
    sources: HashMap<GraphicKind, HashMap<String, Vec<usize>>>,
    generated: Mutex<GeneratedGraphics>,
}

impl Graphics {
    pub fn prepare(&mut self, kind: GraphicKind, source: &str, offset: usize) {
        let has_capacity = self.sources.values().map(HashMap::len).sum::<usize>() < 128;
        let sources = self.sources.entry(kind).or_default();
        if let Some(offsets) = sources.get_mut(source) {
            offsets.push(offset);
        } else if has_capacity {
            sources.insert(source.to_owned(), vec![offset]);
        }
    }

    pub(crate) fn observe(&self, reader: WeakEntity<DocReader>) {
        self.generated.lock().unwrap().reader = Some(reader);
    }

    pub(crate) fn reuse_ready(&self, previous: &Self, remap: impl Fn(usize) -> Option<usize>) {
        let previous = previous.generated.lock().unwrap();
        let mut generated = self.generated.lock().unwrap();
        generated.images.extend(
            previous
                .images
                .iter()
                .filter(|(request, state)| {
                    matches!(state, GraphicState::Ready(_))
                        && self
                            .sources
                            .get(&request.kind)
                            .is_some_and(|sources| sources.contains_key(request.source.as_ref()))
                })
                .map(|(request, state)| (request.clone(), state.clone())),
        );
        let offsets: HashSet<_> = self
            .sources
            .get(&GraphicKind::Diagram)
            .into_iter()
            .flat_map(|sources| sources.values().flatten().copied())
            .collect();
        for (&old, &width) in &previous.diagram_widths {
            if let Some(offset) = remap(old)
                && offsets.contains(&offset)
            {
                generated.diagram_widths.insert(offset, width);
                if let Some(diagram) = previous.diagrams.get(&old) {
                    generated.diagrams.insert(offset, diagram.clone());
                }
            }
        }
    }

    fn state(&self, request: &GraphicRequest) -> GraphicState {
        if !self
            .sources
            .get(&request.kind)
            .is_some_and(|sources| sources.contains_key(request.source.as_ref()))
        {
            return GraphicState::LimitReached;
        }
        self.generated
            .lock()
            .unwrap()
            .images
            .get(request)
            .cloned()
            .unwrap_or(GraphicState::Pending)
    }

    fn request(self: &Arc<Self>, request: &GraphicRequest, cx: &mut App) {
        let mut generated = self.generated.lock().unwrap();
        if generated.pending >= 2 || generated.images.contains_key(request) {
            return;
        }
        if request.diagram_fonts.is_some() {
            generated.images.retain(|key, state| {
                key.diagram_fonts.is_none()
                    || key.diagram_fonts == request.diagram_fonts
                    || matches!(state, GraphicState::Pending)
            });
        }
        generated
            .images
            .insert(request.clone(), GraphicState::Pending);
        generated.pending += 1;
        drop(generated);

        let graphics = Arc::downgrade(self);
        let request = request.clone();
        let text_system = request
            .diagram_fonts
            .as_ref()
            .map(|_| cx.text_system().clone());
        let renderer = cx.svg_renderer();
        cx.spawn(async move |cx| {
            let work = request.clone();
            let picture = cx
                .background_executor()
                .spawn(async move {
                    let picture = match work.kind {
                        GraphicKind::Diagram => mermaid::diagram_with_fonts(
                            &work.source,
                            work.dark,
                            work.reflowed,
                            work.diagram_fonts.zip(text_system),
                        ),
                        _ => formula(
                            &work.source,
                            work.dark,
                            work.kind == GraphicKind::InlineMath,
                        ),
                    };
                    picture
                        .and_then(|picture| {
                            Ok(Picture {
                                image: Arc::new(renderer.parse_svg(picture.image.bytes())?),
                                width: picture.width,
                                height: picture.height,
                                baseline: picture.baseline,
                                reflowable: picture.reflowable,
                            })
                        })
                        .map_err(|error| format!("{error:#}").into())
                })
                .await;
            if let Some(graphics) = graphics.upgrade() {
                let offsets = &graphics.sources[&request.kind][request.source.as_ref()];
                let reader = {
                    let mut generated = graphics.generated.lock().unwrap();
                    generated
                        .images
                        .insert(request, GraphicState::Ready(picture));
                    generated.pending -= 1;
                    generated.reader.clone()
                };
                if let Some(reader) = reader {
                    cx.update(|cx| {
                        _ = reader
                            .update(cx, |reader, cx| reader.refresh_graphic_layout(offsets, cx));
                    });
                }
            }
        })
        .detach();
    }

    fn diagram_width(&self, offset: usize) -> f32 {
        self.generated
            .lock()
            .unwrap()
            .diagram_widths
            .get(&offset)
            .copied()
            .unwrap_or_default()
    }

    fn observe_diagram_width(&self, offset: usize, width: f32, window: &Window, cx: &mut App) {
        let reader = {
            let mut generated = self.generated.lock().unwrap();
            if generated
                .diagram_widths
                .get(&offset)
                .is_some_and(|old| (old - width).abs() < 0.5)
            {
                return;
            }
            generated.diagram_widths.insert(offset, width);
            generated.reader.clone()
        };
        if let Some(reader) = reader {
            window.defer(cx, move |_, cx| {
                _ = reader.update(cx, |reader, cx| {
                    reader.refresh_graphic_layout(&[offset], cx)
                });
            });
        }
    }

    fn placeholder(
        self: &Arc<Self>,
        request: GraphicRequest,
        width: f32,
        height: f32,
    ) -> impl IntoElement + use<> {
        let graphics = self.clone();
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                if !bounds.intersect(&window.content_mask().bounds).is_empty() {
                    graphics.request(&request, cx);
                }
            },
        )
        .w(px(width))
        .h(px(height))
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

impl GraphicPlugin {
    fn request(&self, node: &MarkdownNode, window: &Window, cx: &App) -> GraphicRequest {
        GraphicRequest {
            kind: self.kind,
            source: node.as_text().to_owned().into(),
            dark: cx.theme().mode == ThemeMode::Dark,
            reflowed: false,
            diagram_fonts: (self.kind == GraphicKind::Diagram)
                .then(|| mermaid::DiagramFonts::inherit(window, cx)),
        }
    }

    fn render_diagram(
        &self,
        node: &MarkdownNode,
        request: GraphicRequest,
        cx: &App,
    ) -> gpui::AnyElement {
        let offset = node.source_range().map_or(0, |range| range.start);
        let available = self.graphics.diagram_width(offset);
        let fallback = self
            .graphics
            .generated
            .lock()
            .unwrap()
            .diagrams
            .get(&offset)
            .filter(|diagram| {
                diagram.dark == request.dark && diagram.fonts == request.diagram_fonts
            })
            .cloned();
        let mut next = None;
        let mut completed = None;
        let picture = match self.graphics.state(&request) {
            GraphicState::Pending => {
                next = Some(request);
                None
            }
            GraphicState::Ready(Ok(original)) if available > 0. => {
                let reflowed = if mermaid::should_reflow(available, &original) {
                    let reflow = GraphicRequest {
                        reflowed: true,
                        ..request.clone()
                    };
                    match self.graphics.state(&reflow) {
                        GraphicState::Ready(Ok(picture)) => Some(picture),
                        GraphicState::Pending => {
                            next = Some(reflow);
                            None
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                if next.is_none() || fallback.is_none() {
                    // Keep the original layout available while a first-time reflow runs.
                    // A previously published layout stays in place on subsequent updates.
                    let (picture, scale) = mermaid::fit(available, original, reflowed);
                    let presentation = DiagramPresentation {
                        picture,
                        scale,
                        width: available,
                        dark: request.dark,
                        fonts: request.diagram_fonts,
                    };
                    if next.is_none() {
                        completed = Some(presentation.clone());
                    }
                    Some(Ok(presentation))
                } else {
                    None
                }
            }
            GraphicState::Ready(Ok(_)) => None,
            state => Some(Err(state)),
        };
        let picture = match picture.or_else(|| fallback.map(Ok)) {
            None => div().h(px(24.)).into_any_element(),
            Some(Err(state)) => graphic_error(node.as_text(), state, cx).into_any_element(),
            Some(Ok(diagram)) => {
                let width = diagram.picture.width * diagram.scale;
                let height = diagram.picture.height * diagram.scale;
                let overflows = width > diagram.width + 0.5;
                let image = div()
                    .flex_shrink_0()
                    .child(
                        diagram
                            .picture
                            .element(diagram.scale)
                            .id(("document-mermaid-raster", offset)),
                    )
                    .when(!overflows, |image| image.mx_auto());
                div()
                    .flex()
                    .w_full()
                    .min_w_0()
                    .h(px(height + if overflows { 8. } else { 0. }))
                    .overflow_x_content_scrollbar()
                    .id(("document-mermaid-scroll", offset))
                    .child(image)
                    .into_any_element()
            }
        };
        let graphics = self.graphics.clone();
        let measure = canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                if !bounds.intersect(&window.content_mask().bounds).is_empty() {
                    if let Some(diagram) = &completed {
                        graphics
                            .generated
                            .lock()
                            .unwrap()
                            .diagrams
                            .insert(offset, diagram.clone());
                    }
                    graphics.observe_diagram_width(
                        offset,
                        f32::from(bounds.size.width),
                        window,
                        cx,
                    );
                    if let Some(request) = &next {
                        graphics.request(request, cx);
                    }
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .relative()
            .w_full()
            .min_w_0()
            .child(picture)
            .child(measure)
            .into_any_element()
    }
}

fn graphic_error(source: &str, state: GraphicState, cx: &App) -> impl IntoElement {
    let error = match state {
        GraphicState::Ready(Err(error)) => error,
        _ => t!("docs.graphics_limit").into(),
    };
    div()
        .w_full()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
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

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let request = self.request(node, window, cx);
        if self.kind == GraphicKind::Diagram {
            return self.render_diagram(node, request, cx);
        }
        let inner = match self.graphics.state(&request) {
            GraphicState::Pending => self
                .graphics
                .placeholder(request, 48., 24.)
                .into_any_element(),
            GraphicState::Ready(Ok(picture)) => {
                let font_size = window.text_style().font_size.to_pixels(window.rem_size());
                let scale = f32::from(font_size) / MATH_FONT_SIZE;
                div()
                    .flex()
                    .w_full()
                    .min_w_0()
                    .h(px(picture.height * scale + 8.))
                    .overflow_x_content_scrollbar()
                    .id((
                        "document-graphic",
                        node.source_range().map_or(0, |range| range.start),
                    ))
                    .child(
                        div()
                            .mx_auto()
                            .flex_shrink_0()
                            .child(picture.element(scale)),
                    )
                    .into_any_element()
            }
            state => graphic_error(node.as_text(), state, cx).into_any_element(),
        };
        // Display math sits directly on the reading surface.
        h_flex()
            .w_full()
            .min_w_0()
            .justify_center()
            .child(inner)
            .into_any_element()
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let request = self.request(node, window, cx);
        let font_size = f32::from(context.font_size());
        match self.graphics.state(&request) {
            GraphicState::Pending => Some(
                InlineElement::new(self.graphics.placeholder(request, font_size, font_size))
                    .with_baseline(px(font_size * 0.8)),
            ),
            GraphicState::Ready(Ok(picture)) => {
                let scale = font_size / MATH_FONT_SIZE;
                Some(
                    InlineElement::new(picture.element(scale))
                        .with_baseline(px(picture.baseline * scale)),
                )
            }
            GraphicState::LimitReached | GraphicState::Ready(Err(_)) => None,
        }
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
    let padding = if inline { 1. } else { 4. };
    let svg = render_to_svg_with_color_syntax(
        &list,
        &SvgOptions {
            font_size: f64::from(MATH_FONT_SIZE),
            padding,
            embed_glyphs: true,
            ..SvgOptions::default()
        },
        SvgColorSyntax::Rgb,
    );
    let (width, height) = svg_size(&svg)?;
    Ok(Picture {
        image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
        width,
        height,
        baseline: (list.height * f64::from(MATH_FONT_SIZE) + padding) as f32,
        reflowable: false,
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
            let image = mermaid::diagram(source, false, false).unwrap().image;
            image.to_image_data(rasterizer.clone()).unwrap();
            count += 1;
        }
        assert_eq!(count, 7);
    }

    #[test]
    fn mermaid_preview_keeps_the_published_layout_for_shifted_and_edited_blocks() -> Result<()> {
        let original = "flowchart LR; A --> B";
        let mut previous = Graphics::default();
        previous.prepare(GraphicKind::Diagram, original, 12);
        let picture = mermaid::diagram(original, false, false)?;
        let renderer = gpui::SvgRenderer::new(Arc::new(()));
        let svg = Arc::new(renderer.parse_svg(picture.image.bytes())?);
        {
            let mut generated = previous.generated.lock().unwrap();
            generated.diagram_widths.insert(12, 443.);
            generated.diagrams.insert(
                12,
                DiagramPresentation {
                    picture: Picture {
                        image: svg.clone(),
                        width: picture.width,
                        height: picture.height,
                        baseline: picture.baseline,
                        reflowable: picture.reflowable,
                    },
                    width: 443.,
                    scale: 0.75,
                    dark: false,
                    fonts: None,
                },
            );
        }

        for source in [original, "flowchart LR; A --> C"] {
            let mut replacement = Graphics::default();
            replacement.prepare(GraphicKind::Diagram, source, 22);
            replacement.reuse_ready(&previous, |offset| Some(offset + 10));
            assert_eq!(replacement.diagram_width(22), 443.);
            let generated = replacement.generated.lock().unwrap();
            let presentation = &generated.diagrams[&22];
            assert_eq!(presentation.scale, 0.75);
            assert!(Arc::ptr_eq(&presentation.picture.image, &svg));
            assert!(!generated.diagrams.contains_key(&12));
        }
        Ok(())
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
