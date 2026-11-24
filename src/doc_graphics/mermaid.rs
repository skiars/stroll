use std::{
    borrow::Cow,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use anyhow::Result;
use gpui::{
    App, FontFallbacks, FontStyle, FontWeight, Image, ImageFormat, SharedString, TextRun,
    TextSystem, Window, WindowTextSystem, px,
};
use merman::{
    MermaidConfig, OperationControl, RenderOutput, RenderRequest, Renderer, SourceSpan,
    SvgEnvironment, SvgRequest, preprocess_diagram,
    svg::{
        DeterministicTextMeasurer, HostTheme, HostThemePreset, MeasurementProfileId, Presentation,
        PresentationProfile, RenderResourcePolicy, ScopedCssPostprocessor, SvgPipeline,
        SvgPostprocessContext, SvgPostprocessor, TextMeasurementPolicy, TextMeasurementProfile,
        TextMeasurementProfileIdentity, ThemeRole,
    },
};
use regex::Regex;
use rust_i18n::t;

use super::{Picture, svg_size};

const MIN_SCALE: f32 = 0.75;
const STATE_EDGE_WRAP_WIDTH: i32 = 200;
const STATE_EDGE_REFLOW_WIDTH: i32 = 84;

pub(super) fn should_reflow<T>(available: f32, picture: &Picture<T>) -> bool {
    picture.reflowable && available > 0. && available < picture.width * MIN_SCALE
}

pub(super) fn fit<T>(
    available: f32,
    original: Picture<T>,
    reflowed: Option<Picture<T>>,
) -> (Picture<T>, f32) {
    if available <= 0. {
        return (original, 1.);
    }
    let picture = reflowed
        .filter(|candidate| should_reflow(available, &original) && candidate.width < original.width)
        .unwrap_or(original);
    let scale = (available / picture.width).clamp(MIN_SCALE, 1.);
    (picture, scale)
}

const LIGHT: Palette = Palette {
    surface: "#ffffff",
    node: "#f4f6fb",
    highlight: "#edf0ff",
    text: "#303540",
    muted: "#535f75",
    border: "#a9b3cb",
    accent: "#4f6bed",
    line: "#657188",
    group_border: "#e4e4df",
};
const DARK: Palette = Palette {
    surface: "#1c1d22",
    node: "#272b38",
    highlight: "#2c3354",
    text: "#e0e3ef",
    muted: "#bdc6dd",
    border: "#65708f",
    accent: "#8ea2ff",
    line: "#a3acc5",
    group_border: "#3d3e46",
};

#[derive(Clone, Copy)]
struct Palette {
    surface: &'static str,
    node: &'static str,
    highlight: &'static str,
    text: &'static str,
    muted: &'static str,
    border: &'static str,
    accent: &'static str,
    line: &'static str,
    group_border: &'static str,
}

impl Palette {
    fn theme(self, mut theme: HostTheme) -> Result<HostTheme> {
        for (role, value) in [
            (ThemeRole::Canvas, self.surface),
            (ThemeRole::Surface, self.node),
            (ThemeRole::SurfaceAlt, self.highlight),
            (ThemeRole::Text, self.text),
            (ThemeRole::SubtleText, self.muted),
            (ThemeRole::Border, self.border),
            (ThemeRole::Line, self.line),
            (ThemeRole::EdgeLabelBackground, self.surface),
            (ThemeRole::ClusterBackground, self.surface),
            (ThemeRole::ClusterBorder, self.group_border),
            (ThemeRole::NoteBackground, self.highlight),
            (ThemeRole::NoteBorder, self.border),
            (ThemeRole::NoteText, self.text),
            (ThemeRole::ActorBackground, self.node),
            (ThemeRole::ActorBorder, self.border),
            (ThemeRole::ActorText, self.text),
        ] {
            theme = theme.try_with_role(role, value)?;
        }
        Ok(theme)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct DiagramFonts {
    body: SharedString,
    mono: SharedString,
    fallbacks: Option<FontFallbacks>,
}

impl DiagramFonts {
    pub(super) fn inherit(window: &Window, cx: &App) -> Self {
        let style = window.text_style();
        let system = window.text_system();
        let body = style.font();
        let mut mono = gpui::font(crate::workspace::ui_code_font(cx));
        mono.fallbacks = style.font_fallbacks.clone();
        let resolved_family = |font: gpui::Font| {
            system
                .get_font_for_id(system.resolve_font(&font))
                .map_or(font.family, |resolved| resolved.family)
        };
        Self {
            body: resolved_family(body),
            mono: resolved_family(mono),
            fallbacks: style.font_fallbacks,
        }
    }

    fn body_stack(&self) -> String {
        let system = if cfg!(target_os = "macos") {
            r#"".AppleSystemUIFont", ".SF NS", "SF Pro Text", "Helvetica Neue""#.to_owned()
        } else if cfg!(target_os = "windows") {
            r#""Segoe UI""#.to_owned()
        } else {
            r#""IBM Plex Sans""#.to_owned()
        };
        let primary = if self.body == ".SystemUIFont" {
            Some(system)
        } else {
            css_family(gpui::font_name_with_fallbacks(&self.body, ""))
        };
        primary
            .into_iter()
            .chain(self.fallbacks.iter().flat_map(|fonts| {
                fonts
                    .fallback_list()
                    .iter()
                    .filter_map(|name| css_family(name))
            }))
            .chain(["sans-serif".to_owned()])
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn mono_stack(&self) -> String {
        css_family(gpui::font_name_with_fallbacks(&self.mono, "")).map_or_else(
            || "monospace".to_owned(),
            |family| format!("{family}, monospace"),
        )
    }

    fn measurement(self, text_system: Arc<TextSystem>) -> Result<TextMeasurementPolicy> {
        let body_stack = self.body_stack();
        let text = Mutex::new(WindowTextSystem::new(text_system));
        let widths =
            DeterministicTextMeasurer::default().with_width_callback(move |value, style| {
                let family = style.font_family.as_deref();
                let family = match family {
                    None => self.body.as_str(),
                    Some(family) if family == body_stack => self.body.as_str(),
                    Some("monospace" | "ui-monospace") => self.mono.as_str(),
                    Some(family) => family
                        .split(',')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .trim_matches(['"', '\'']),
                };
                let mut font = gpui::font(family.to_owned());
                font.fallbacks = self.fallbacks.clone();
                font.style = match style.font_style.as_deref() {
                    Some("italic") => FontStyle::Italic,
                    Some("oblique") => FontStyle::Oblique,
                    _ => FontStyle::Normal,
                };
                font.weight = match style.font_weight.as_deref() {
                    Some("bold" | "bolder") => FontWeight::BOLD,
                    Some("lighter") => FontWeight::LIGHT,
                    Some(weight) => weight.parse::<f32>().map(FontWeight).unwrap_or_default(),
                    None => FontWeight::NORMAL,
                };
                let run = TextRun {
                    len: value.len(),
                    font,
                    ..TextRun::default()
                };
                f64::from(f32::from(
                    text.lock()
                        .unwrap()
                        .layout_line(value, px(style.font_size as f32), &[run], None)
                        .width,
                ))
            });
        let identity = TextMeasurementProfileIdentity::new(
            MeasurementProfileId::new("stroll-document-fonts")?,
            "1",
        )?;
        Ok(TextMeasurementPolicy::uniform(TextMeasurementProfile::new(
            identity, widths,
        )))
    }
}

fn css_family(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()
        && !name
            .chars()
            .any(|ch| ch.is_control() || "\"\\<>&{};".contains(ch)))
    .then(|| format!(r#""{name}""#))
}

fn horizontal_direction(diagram: &merman::PreprocessResult, source: &str) -> Option<SourceSpan> {
    static HEADER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A\s*(?:flowchart|graph)[ \t]+(LR|RL)\b").unwrap());
    let config = diagram.config.as_value();
    if config.get("layout").is_some()
        || config.get("elk").is_some()
        || config.pointer("/flowchart/layout").is_some()
        || config.pointer("/flowchart/defaultRenderer").is_some()
    {
        return None;
    }
    let direction = HEADER.captures(diagram.code())?.get(1)?;
    let span = diagram
        .source
        .try_map_span(SourceSpan::new(direction.start(), direction.end()))?;
    (source.get(span.start..span.end) == Some(direction.as_str())).then_some(span)
}

#[cfg(test)]
pub(super) fn diagram(source: &str, dark: bool, reflowed: bool) -> Result<Picture> {
    diagram_with_fonts(source, dark, reflowed, None)
}

pub(super) fn diagram_with_fonts(
    source: &str,
    dark: bool,
    reflowed: bool,
    fonts: Option<(DiagramFonts, Arc<TextSystem>)>,
) -> Result<Picture> {
    let engine = merman::Engine::new();
    let resources = RenderResourcePolicy::interactive();
    resources.input_policy().check_source_bytes(source)?;
    let control = OperationControl::new().with_deadline(Duration::from_secs(2));
    let preprocessed = preprocess_diagram(source, engine.registry())?;
    let direction = horizontal_direction(&preprocessed, source);
    let mut vertical_source = String::new();
    let source = if let Some(span) = direction.filter(|_| reflowed) {
        vertical_source.push_str(&source[..span.start]);
        vertical_source.push_str("TD");
        vertical_source.push_str(&source[span.end..]);
        vertical_source.as_str()
    } else {
        source
    };

    let mut configuration = MermaidConfig::empty_object();
    let kind = engine
        .registry()
        .detect_type(preprocessed.code(), &mut configuration)
        .ok();
    let flowchart = matches!(kind, Some("flowchart-v2" | "flowchart" | "flowchart-elk"));
    let state = kind == Some("stateDiagram");
    anyhow::ensure!(
        !reflowed || direction.is_some() || state,
        t!("docs.no_mermaid")
    );
    let original_config = preprocessed.config.as_value();
    let (preset, palette) = if dark {
        (HostThemePreset::EditorDark, DARK)
    } else {
        (HostThemePreset::EditorLight, LIGHT)
    };
    let mut theme = HostTheme::from_preset(preset);
    let mut pipeline = SvgPipeline::resvg_safe();
    let mut environment = SvgEnvironment::deterministic().with_resource_policy(resources);
    if let Some((fonts, text_system)) = fonts {
        let authored_family = original_config
            .get("fontFamily")
            .or_else(|| original_config.pointer("/themeVariables/fontFamily"))
            .and_then(|value| value.as_str());
        theme = theme.try_with_font_family(fonts.body_stack())?;
        if let Some(family) = authored_family
            && let Ok(authored) = theme.clone().try_with_font_family(family)
        {
            theme = authored;
        }
        let monospace = fonts.mono_stack();
        pipeline = pipeline.with_postprocessor(ScopedCssPostprocessor::new(format!(
            "code,pre{{font-family:{monospace};}}.console-glyph{{font-family:{monospace} !important;}}"
        )));
        environment = environment.with_text_measurement_policy(fonts.measurement(text_system)?);
    }
    let mut defaults = MermaidConfig::empty_object();
    if flowchart || state || kind == Some("sequence") {
        theme = palette.theme(theme)?;
    }
    if flowchart {
        defaults.set_value("flowchart.padding", 12.into());
        defaults.set_value("flowchart.readerLabelPadding", true.into());
        defaults.set_value("flowchart.minNodeWidth", 96.into());
        if original_config
            .pointer("/flowchart/wrappingWidth")
            .is_none()
        {
            defaults.set_value("flowchart.explicitLineBreakWrappingWidth", 280.into());
        }
        let host_colors = original_config.get("theme").is_none()
            && original_config.get("themeVariables").is_none()
            && original_config.pointer("/flowchart/theme").is_none();
        pipeline = pipeline.with_postprocessor(FlowchartPaint(host_colors.then_some(palette)));
    }
    if state {
        let wrap_width = if reflowed {
            STATE_EDGE_REFLOW_WIDTH
        } else {
            STATE_EDGE_WRAP_WIDTH
        };
        defaults.set_value("state.readerEdgeLabelWrappingWidth", wrap_width.into());
    }
    let presentation = Presentation::new()
        .with_profile(PresentationProfile::MermanModern)
        .with_theme(theme)
        .resolve();
    let renderer = Renderer::new()
        .with_engine(presentation.materialize_engine(engine.with_site_config(defaults)));
    let request = SvgRequest {
        environment,
        pipeline: Some(pipeline),
        presentation: presentation.render_policy().with_reader_flowchart_style(),
        ..SvgRequest::default()
    };
    let output = renderer.render(RenderRequest::svg(source, control, request))?;
    let RenderOutput::Svg(Some(output)) = output else {
        anyhow::bail!(t!("docs.no_mermaid"));
    };
    let (svg, _) = output.into_parts();
    let (width, height) = svg_size(&svg)?;
    Ok(Picture {
        image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
        width,
        height,
        baseline: 0.,
        reflowable: (direction.is_some() || state) && !reflowed,
    })
}

struct FlowchartPaint(Option<Palette>);

impl SvgPostprocessor for FlowchartPaint {
    fn name(&self) -> &'static str {
        "reader-flowchart-paint"
    }

    fn process<'a>(
        &self,
        svg: Cow<'a, str>,
        ctx: &SvgPostprocessContext<'_>,
    ) -> merman::svg::RenderResult<Cow<'a, str>> {
        let xml = roxmltree::Document::parse(&svg).map_err(|error| {
            merman::svg::RenderError::svg_postprocess(self.name(), error.to_string())
        })?;
        let mut edits = Vec::new();
        for rect in xml.descendants().filter(|node| node.has_tag_name("rect")) {
            ctx.checkpoint()?;
            let class = rect.attribute("class").unwrap_or_default();
            let parent = rect
                .parent_element()
                .and_then(|node| node.attribute("class"))
                .unwrap_or_default();
            let node_classes = parent.split_ascii_whitespace().collect::<Vec<_>>();
            if class
                .split_ascii_whitespace()
                .any(|value| value == "label-container")
            {
                let decision = node_classes.contains(&"stroll-decision");
                let (radius, fill, stroke) = if decision {
                    (9., self.0.map(|p| p.highlight), self.0.map(|p| p.accent))
                } else if node_classes.contains(&"stroll-decision-authored") {
                    (9., None, None)
                } else if node_classes.contains(&"stroll-entry") {
                    (24., self.0.map(|p| p.highlight), None)
                } else if node_classes.contains(&"stroll-process") {
                    (7., None, None)
                } else {
                    continue;
                };
                if rect.attribute("width").is_none() || rect.attribute("height").is_none() {
                    continue;
                }
                // Authored declarations live in the existing inline style and retain precedence.
                let mut style = String::new();
                if let Some(fill) = fill {
                    style.push_str(&format!("fill:{fill};"));
                }
                if let Some(stroke) = stroke {
                    style.push_str(&format!("stroke:{stroke};"));
                }
                if decision || node_classes.contains(&"stroll-decision-authored") {
                    style.push_str("stroke-dasharray:4 3;");
                }
                geometry_edits(&svg, rect, radius, &style, &mut edits);
            } else if class == "background"
                && rect
                    .ancestors()
                    .any(|node| node.attribute("class") == Some("edgeLabel"))
            {
                let height = rect
                    .attribute("height")
                    .and_then(|value| value.parse::<f32>().ok())
                    .unwrap_or_default();
                if height > 0. && rect.attribute("width").is_some() {
                    let style = self.0.map_or_else(String::new, |p| {
                        format!("stroke:{};stroke-width:1;", p.border)
                    });
                    geometry_edits(&svg, rect, height * 0.5, &style, &mut edits);
                }
            }
        }
        if edits.is_empty() {
            return Ok(svg);
        }
        edits.sort_unstable_by_key(|(range, _)| range.start);
        let mut output = String::with_capacity(svg.len());
        let mut start = 0;
        for (range, replacement) in edits {
            output.push_str(&svg[start..range.start]);
            output.push_str(&replacement);
            start = range.end;
        }
        output.push_str(&svg[start..]);
        Ok(Cow::Owned(output))
    }
}

fn geometry_edits(
    svg: &str,
    rect: roxmltree::Node<'_, '_>,
    radius: f32,
    style: &str,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
) {
    let mut attributes = String::new();
    for name in ["rx", "ry"] {
        if let Some(attribute) = rect.attribute_node(name) {
            edits.push((attribute.range_value(), radius.to_string()));
        } else {
            attributes.push_str(&format!(r#" {name}="{radius}""#));
        }
    }
    if !style.is_empty() {
        if let Some(attribute) = rect.attribute_node("style") {
            let offset = attribute.range_value().start;
            edits.push((offset..offset, style.to_owned()));
        } else {
            attributes.push_str(&format!(r#" style="{style}""#));
        }
    }
    if !attributes.is_empty() {
        let start = rect.range().start + "<rect".len();
        if svg.is_char_boundary(start) {
            edits.push((start..start, attributes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_reflow_changes_only_the_header_direction() {
        let source =
            "---\ntitle: Review\n---\n%% flowchart RL\nflowchart LR; A[\"graph RL\"] --> B";
        let parsed = preprocess_diagram(source, merman::Engine::new().registry()).unwrap();
        let direction = horizontal_direction(&parsed, source).unwrap();
        assert_eq!(&source[direction.start..direction.end], "LR");
        let explicit = "---\nconfig:\n  layout: elk\n---\nflowchart LR\nA --> B";
        let parsed = preprocess_diagram(explicit, merman::Engine::new().registry()).unwrap();
        assert!(horizontal_direction(&parsed, explicit).is_none());
    }

    #[test]
    fn mermaid_reader_wraps_state_transitions_before_fitting_to_width() {
        let fixture = include_str!("../../tests/fixtures/mermaid-layout.md");
        let (_, state) = fixture
            .split_once("## State diagram with parallel transitions\n\n```mermaid\n")
            .unwrap();
        let (state, _) = state.split_once("\n```").unwrap();
        let original = diagram(state, false, false).unwrap();
        let wrapped = diagram(state, false, true).unwrap();
        assert!(
            wrapped.width < original.width,
            "{} >= {}",
            wrapped.width,
            original.width
        );

        let bounds = [original.width, original.width * 0.75, 480., 160.];
        assert!(!should_reflow(bounds[0], &original));
        assert!(!should_reflow(bounds[1], &original));
        assert!(should_reflow(bounds[2], &original));
        let (picture, scale) = fit(bounds[0], original.clone(), Some(wrapped.clone()));
        assert_eq!(picture.width, original.width);
        assert_eq!(scale, 1.);
        let (picture, scale) = fit(bounds[2], original.clone(), Some(wrapped.clone()));
        assert_eq!(picture.width, wrapped.width);
        assert!(picture.width * scale <= bounds[2] + 0.01);
        let (picture, scale) = fit(bounds[3], original, Some(wrapped.clone()));
        assert_eq!(picture.width, wrapped.width);
        assert_eq!(scale, 0.75);
        assert!(picture.width * scale > bounds[3]);

        let xml = roxmltree::Document::parse(std::str::from_utf8(wrapped.image.bytes()).unwrap())
            .unwrap();
        let labels = xml
            .descendants()
            .filter(|node| {
                node.has_tag_name("text")
                    && node
                        .attribute("data-merman-source-classes")
                        .is_some_and(|classes| {
                            classes
                                .split_ascii_whitespace()
                                .any(|class| class == "edgeLabels")
                        })
            })
            .collect::<Vec<_>>();
        let text = labels
            .iter()
            .filter_map(|node| node.text())
            .collect::<Vec<_>>()
            .join(" ");
        for label in [
            "Reader checks out the book",
            "Reader returns the book",
            "Reader places a hold",
            "Hold expires",
            "Reader cancels the hold",
        ] {
            assert!(text.contains(label), "{label}: {text}");
        }
        assert!(labels.len() > 5, "{text}");
        for label in labels {
            let viewport = label
                .ancestors()
                .skip(1)
                .find(|node| node.has_tag_name("svg"))
                .unwrap();
            assert!(
                viewport
                    .children()
                    .filter(|node| node.has_tag_name("rect"))
                    .all(|node| { node.attribute("fill") == Some("none") })
            );
        }

        for picture in [
            wrapped,
            diagram(
                &format!("---\nconfig:\n  layout: elk\n---\n{state}"),
                false,
                true,
            )
            .unwrap(),
        ] {
            let svg = std::str::from_utf8(picture.image.bytes()).unwrap();
            let xml = roxmltree::Document::parse(svg).unwrap();
            let edges = xml
                .descendants()
                .filter(|node| {
                    node.has_tag_name("path") && node.attribute("data-edge") == Some("true")
                })
                .collect::<Vec<_>>();
            assert_eq!(edges.len(), 6);
            assert!(
                edges
                    .iter()
                    .all(|node| node.attribute("marker-end").is_some())
            );
            assert_eq!(
                edges
                    .iter()
                    .filter(|node| node.attribute("d").unwrap().matches('M').count() > 1)
                    .count(),
                5,
                "{:?}",
                edges
                    .iter()
                    .map(|node| node.attribute("d"))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mermaid_reader_inherits_document_font_families() -> Result<()> {
        let text = Arc::new(TextSystem::new(
            gpui_platform::current_platform(true).text_system(),
        ));
        let installed = text.all_font_names();
        let matching_pair = [
            ("DejaVu Sans", "DejaVu Sans Mono"),
            ("Liberation Sans", "Liberation Mono"),
        ]
        .into_iter()
        .find(|(body, mono)| {
            installed.iter().any(|family| family == body)
                && installed.iter().any(|family| family == mono)
        });
        let Some((body, mono)) = matching_pair else {
            return Ok(());
        };
        let body_label = "iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii";
        let source = format!("flowchart TD\nA[\"{body_label}\"] --> B[\"<code>CodeOnly</code>\"]");
        let mut widths = Vec::new();
        for (body, mono) in [(body, mono), (mono, body)] {
            let fonts = DiagramFonts {
                body: body.into(),
                mono: mono.into(),
                fallbacks: None,
            };
            let picture =
                diagram_with_fonts(&source, false, false, Some((fonts.clone(), text.clone())))?;
            widths.push(picture.width);
            let svg = std::str::from_utf8(picture.image.bytes())?;
            let xml = roxmltree::Document::parse(svg)?;
            for (label, family) in [("iiiiiiii", body), ("CodeOnly", mono)] {
                let style = xml
                    .descendants()
                    .find(|node| {
                        node.has_tag_name("text")
                            && node.text().is_some_and(|text| text.contains(label))
                    })
                    .and_then(|node| node.attribute("style"))
                    .unwrap();
                assert!(style.contains(family), "{style}");
            }

            let explicit =
                format!("---\nconfig:\n  themeVariables:\n    fontFamily: {mono}\n---\n{source}");
            let explicit =
                diagram_with_fonts(&explicit, false, false, Some((fonts, text.clone())))?;
            let svg = std::str::from_utf8(explicit.image.bytes())?;
            let xml = roxmltree::Document::parse(svg)?;
            let style = xml
                .descendants()
                .find(|node| {
                    node.has_tag_name("text")
                        && node.text().is_some_and(|text| text.contains("iiiiiiii"))
                })
                .and_then(|node| node.attribute("style"))
                .unwrap();
            assert!(style.contains(mono), "{style}");
        }
        assert!(widths[1] > widths[0] + 30., "{widths:?}");
        Ok(())
    }

    #[test]
    fn mermaid_reader_splits_elk_edges_at_their_capsules() {
        let source = "flowchart LR\nA <-->|Review| B\nB -. Retry .-> C\nC --> D";
        let source = format!("---\nconfig:\n  layout: elk\n---\n{source}");
        for dark in [false, true] {
            let picture = diagram(&source, dark, false).unwrap();
            let svg = std::str::from_utf8(picture.image.bytes()).unwrap();
            let xml = roxmltree::Document::parse(svg).unwrap();
            let paths = |id| {
                xml.descendants()
                    .filter(|node| {
                        node.has_tag_name("path") && node.attribute("data-id") == Some(id)
                    })
                    .collect::<Vec<_>>()
            };
            for (id, start_count) in [("L_A_B_0", 1), ("L_B_C_0", 0)] {
                let pieces = paths(id);
                assert_eq!(pieces.len(), 2, "{id}: {pieces:?}");
                assert_eq!(
                    pieces
                        .iter()
                        .filter(|node| node.attribute("data-edge") == Some("true"))
                        .count(),
                    1
                );
                assert_eq!(
                    pieces
                        .iter()
                        .filter(|node| node.attribute("marker-start").is_some())
                        .count(),
                    start_count
                );
                assert!(pieces[0].attribute("marker-end").is_none());
                assert!(pieces[1].attribute("marker-end").is_some());
                assert_ne!(pieces[0].attribute("d"), pieces[1].attribute("d"));
                let style = pieces[1].attribute("style").unwrap();
                assert_eq!(style.matches("stroke-dashoffset:").count(), 1);
                let offset = style
                    .split_once("stroke-dashoffset:")
                    .unwrap()
                    .1
                    .trim_end_matches(';')
                    .parse::<f64>()
                    .unwrap();
                assert!(offset > 0.);
            }
            assert_eq!(paths("L_C_D_0").len(), 1);
            let capsules = xml
                .descendants()
                .filter(|node| {
                    node.has_tag_name("rect") && node.attribute("class") == Some("background")
                })
                .collect::<Vec<_>>();
            assert_eq!(capsules.len(), 2);
            for capsule in capsules {
                let height = capsule.attribute("height").unwrap().parse::<f32>().unwrap();
                let radius = capsule.attribute("rx").unwrap().parse::<f32>().unwrap();
                assert_eq!(radius, height / 2.);
                let stroke = if dark { DARK.border } else { LIGHT.border };
                assert!(
                    capsule
                        .attribute("style")
                        .is_some_and(|style| style.contains(stroke))
                );
            }
        }
    }

    #[test]
    fn mermaid_reader_styles_decisions_and_wraps_only_explicit_breaks() {
        let source = "flowchart TD\nA[Start] -->|Check| B{\"This decision needs to be documented<br/>before publishing the result\"}\nB --> C[Finish]";
        let styled = diagram(source, false, false).unwrap();
        let explicit = format!("---\nconfig:\n  flowchart:\n    wrappingWidth: 120\n---\n{source}");
        let authored = diagram(&explicit, false, false).unwrap();
        assert!(styled.width > authored.width);
        let svg = std::str::from_utf8(styled.image.bytes()).unwrap();
        let xml = roxmltree::Document::parse(svg).unwrap();
        let decision = xml
            .descendants()
            .find(|node| {
                node.attribute("class").is_some_and(|class| {
                    class
                        .split_ascii_whitespace()
                        .any(|class| class == "stroll-decision")
                })
            })
            .unwrap();
        assert!(decision.descendants().any(|node| {
            node.has_tag_name("rect")
                && node.attribute("rx") == Some("9")
                && node
                    .attribute("style")
                    .is_some_and(|style| style.contains("stroke-dasharray:4 3"))
        }));
        assert!(svg.contains("stroke:#a9b3cb"));
    }
}
