use rust_i18n::t;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use gpui::{
    App, Asset, DevicePixels, Image, ImageFormat, IntoElement, ObjectFit, ParentElement as _,
    RenderImage, SharedString, Size, Styled, StyledImage as _, SvgSize, WeakEntity, Window, canvas,
    div, img, px, size,
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
struct Picture {
    image: Arc<Image>,
    width: f32,
    height: f32,
    baseline: f32,
}

const MATH_FONT_SIZE: f32 = 16.;

enum FormulaRaster {}

impl Asset for FormulaRaster {
    type Source = (Arc<Image>, Size<DevicePixels>);
    type Output = Option<Arc<RenderImage>>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            let (image, dimensions) = source;
            let svg = renderer.parse_svg(image.bytes()).ok()?;
            renderer
                .render_parsed(&svg, SvgSize::ExactSize(dimensions))
                .ok()
        }
    }
}

impl Picture {
    fn math_element(&self, scale: f32) -> impl IntoElement + use<> {
        let image = self.image.clone();
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                let width = f32::from(bounds.size.width * window.scale_factor());
                let height = f32::from(bounds.size.height * window.scale_factor());
                let limit = (8192. / width.max(height))
                    .min((16_000_000. / (width * height)).sqrt())
                    .min(1.);
                let dimensions = size(
                    DevicePixels((width * limit).round().max(1.) as i32),
                    DevicePixels((height * limit).round().max(1.) as i32),
                );
                if let Some(Some(raster)) =
                    window.use_asset::<FormulaRaster>(&(image, dimensions), cx)
                {
                    _ = window.paint_image(bounds, bounds, Default::default(), raster, 0, false);
                }
            },
        )
        .w(px(self.width * scale))
        .h(px(self.height * scale))
    }
}

#[derive(Clone)]
enum GraphicState {
    Pending,
    Ready(Result<Picture, SharedString>),
    LimitReached,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct GraphicRequest {
    kind: GraphicKind,
    source: SharedString,
    dark: bool,
}

#[derive(Default)]
struct GeneratedGraphics {
    images: HashMap<GraphicRequest, GraphicState>,
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

    pub(crate) fn reuse_ready(&self, previous: &Self) {
        let images = previous
            .generated
            .lock()
            .unwrap()
            .images
            .iter()
            .filter(|(request, state)| {
                matches!(state, GraphicState::Ready(_))
                    && self
                        .sources
                        .get(&request.kind)
                        .is_some_and(|sources| sources.contains_key(request.source.as_ref()))
            })
            .map(|(request, state)| (request.clone(), state.clone()))
            .collect::<Vec<_>>();
        self.generated.lock().unwrap().images.extend(images);
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
        generated
            .images
            .insert(request.clone(), GraphicState::Pending);
        generated.pending += 1;
        drop(generated);

        let graphics = Arc::downgrade(self);
        let request = request.clone();
        cx.spawn(async move |cx| {
            let work = request.clone();
            let picture = cx
                .background_executor()
                .spawn(async move {
                    let picture = match work.kind {
                        GraphicKind::Diagram => diagram(&work.source, work.dark),
                        _ => formula(
                            &work.source,
                            work.dark,
                            work.kind == GraphicKind::InlineMath,
                        ),
                    };
                    picture.map_err(|error| format!("{error:#}").into())
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
    fn request(&self, node: &MarkdownNode, cx: &App) -> GraphicRequest {
        GraphicRequest {
            kind: self.kind,
            source: node.as_text().to_owned().into(),
            dark: cx.theme().mode == ThemeMode::Dark,
        }
    }
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
        let source = node.as_text();
        let theme = cx.theme();
        let request = self.request(node, cx);
        let inner = match self.graphics.state(&request) {
            GraphicState::Pending => self
                .graphics
                .placeholder(request, 48., 24.)
                .into_any_element(),
            GraphicState::Ready(Ok(picture)) if self.kind == GraphicKind::DisplayMath => {
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
                            .child(picture.math_element(scale)),
                    )
                    .into_any_element()
            }
            GraphicState::Ready(Ok(picture)) => div()
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
                let error = match result {
                    GraphicState::Ready(Err(error)) => error,
                    _ => t!("docs.graphics_limit").into(),
                };
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
        let request = self.request(node, cx);
        let font_size = f32::from(context.font_size());
        match self.graphics.state(&request) {
            GraphicState::Pending => Some(
                InlineElement::new(self.graphics.placeholder(request, font_size, font_size))
                    .with_baseline(px(font_size * 0.8)),
            ),
            GraphicState::Ready(Ok(picture)) => {
                let scale = font_size / MATH_FONT_SIZE;
                Some(
                    InlineElement::new(picture.math_element(scale))
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
