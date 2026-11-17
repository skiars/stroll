//! Code color schemes for editors, diffs, fenced code blocks, and hover documentation.
//!
//! Foreground lightness is adjusted in OKLab to meet contrast targets on code and app
//! backgrounds. The resulting colors map to GPUI syntax captures.

use std::{collections::HashMap, sync::Arc};

use gpui::{Hsla, Rgba};
use gpui_component::{
    ThemeMode,
    highlighter::{HighlightTheme, HighlightThemeStyle},
};
use serde_json::{Map, Value, json};

use crate::config::{CodeContrast, CodeTheme};

const ACTIVE_LINE_ALPHA: u8 = 0x0f;
const SEMANTIC_UNDERLINE_CONTRAST: f64 = 3.;

/// Contrast targets for the code text presets.
fn contrast_target(contrast: CodeContrast) -> f64 {
    match contrast {
        CodeContrast::Standard => 4.5,
        CodeContrast::Enhanced => 6.,
        CodeContrast::High => 7.5,
    }
}

/// Color roles shared by the light and dark palettes.
struct Palette {
    surface: &'static str,
    block_surface: &'static str,
    text: &'static str,
    dim: &'static str,
    warning: &'static str,
    error: &'static str,
    keyword: &'static str,
    flow: &'static str,
    modifier: &'static str,
    literal: &'static str,
    type_fg: &'static str,
    builtin: &'static str,
    interface: &'static str,
    generic: &'static str,
    function: &'static str,
    property: &'static str,
    parameter: &'static str,
    variable: &'static str,
    constant: &'static str,
    namespace: &'static str,
    string: &'static str,
    number: &'static str,
    macro_fg: &'static str,
    preprocessor: Option<&'static str>,
}

const PRISM: [Palette; 2] = [
    Palette {
        surface: "#ffffff",
        block_surface: "#f3f5f8",
        text: "#24242b",
        dim: "#535353",
        warning: "#986300",
        error: "#b52e3d",
        keyword: "#b70c36",
        flow: "#bb102f",
        modifier: "#b70c36",
        literal: "#b70c36",
        type_fg: "#007c4c",
        builtin: "#b70c36",
        interface: "#007c4c",
        generic: "#a43576",
        function: "#7318d9",
        property: "#292832",
        parameter: "#ab4f00",
        variable: "#0054d4",
        constant: "#655014",
        namespace: "#007c4c",
        string: "#0d3e75",
        number: "#0054d4",
        macro_fg: "#7318d9",
        preprocessor: Some("#005b64"),
    },
    Palette {
        surface: "#17191f",
        block_surface: "#20242c",
        text: "#d9dee8",
        dim: "#9fae99",
        warning: "#ebc376",
        error: "#ff9eac",
        keyword: "#d49bff",
        flow: "#ff9988",
        modifier: "#c6d574",
        literal: "#eeb47b",
        type_fg: "#88dba0",
        builtin: "#72d7db",
        interface: "#89bce8",
        generic: "#f49dcb",
        function: "#68d2ed",
        property: "#c8a1e9",
        parameter: "#f6a6bb",
        variable: "#91b5ff",
        constant: "#ff9b96",
        namespace: "#b2a9c5",
        string: "#b8d96d",
        number: "#e49be5",
        macro_fg: "#e9c376",
        preprocessor: None,
    },
];

const ORCHARD: [Palette; 2] = [
    Palette {
        surface: "#ffffff",
        block_surface: "#f3f5f8",
        text: "#303540",
        dim: "#756675",
        warning: "#986300",
        error: "#b52e3d",
        keyword: "#174fa0",
        flow: "#9d261f",
        modifier: "#576b11",
        literal: "#135c9b",
        type_fg: "#942f85",
        builtin: "#6744af",
        interface: "#156885",
        generic: "#a45d10",
        function: "#176b37",
        property: "#a05010",
        parameter: "#8d6314",
        variable: "#953d64",
        constant: "#af3b20",
        namespace: "#76557a",
        string: "#00717b",
        number: "#a26504",
        macro_fg: "#2358a9",
        preprocessor: None,
    },
    Palette {
        surface: "#17191f",
        block_surface: "#20242c",
        text: "#d9dee8",
        dim: "#b7a4b2",
        warning: "#ebc376",
        error: "#ff9eac",
        keyword: "#98b8ff",
        flow: "#ff9b8d",
        modifier: "#c8d780",
        literal: "#8abfff",
        type_fg: "#ee96dd",
        builtin: "#c4a5fa",
        interface: "#7dcbe5",
        generic: "#e9bd7c",
        function: "#93db9c",
        property: "#efb17d",
        parameter: "#e6cd84",
        variable: "#f29fbe",
        constant: "#ffa18b",
        namespace: "#c5a7ce",
        string: "#70d5dc",
        number: "#e9cc72",
        macro_fg: "#91b9ff",
        preprocessor: None,
    },
];

const SPECTRUM: [Palette; 2] = [
    Palette {
        surface: "#ffffff",
        block_surface: "#f3f5f8",
        text: "#20262f",
        dim: "#535353",
        warning: "#986300",
        error: "#b52e3d",
        keyword: "#6929af",
        flow: "#bb1037",
        modifier: "#6929af",
        literal: "#bb1037",
        type_fg: "#006859",
        builtin: "#6929af",
        interface: "#006859",
        generic: "#a43576",
        function: "#182faf",
        property: "#702a54",
        parameter: "#9b4300",
        variable: "#29384b",
        constant: "#a02f27",
        namespace: "#006859",
        string: "#295321",
        number: "#873d81",
        macro_fg: "#811f4a",
        preprocessor: None,
    },
    Palette {
        surface: "#151923",
        block_surface: "#1e2532",
        text: "#d6ddea",
        dim: "#a1adc1",
        warning: "#ebc376",
        error: "#ff9eac",
        keyword: "#d3a1f3",
        flow: "#ff9988",
        modifier: "#c6d574",
        literal: "#f4aa72",
        type_fg: "#76d5bf",
        builtin: "#91d98b",
        interface: "#a6c6e8",
        generic: "#f49dcb",
        function: "#91b3ff",
        property: "#f0a1b8",
        parameter: "#e3ce82",
        variable: "#c7cdd8",
        constant: "#ffad8e",
        namespace: "#b3acc6",
        string: "#b8d96d",
        number: "#e49be5",
        macro_fg: "#e7c477",
        preprocessor: None,
    },
];

const NEON: [Palette; 2] = [
    Palette {
        surface: "#ffffff",
        block_surface: "#f3f5f8",
        text: "#253145",
        dim: "#596879",
        warning: "#986300",
        error: "#b52e3d",
        keyword: "#ac207d",
        flow: "#b43f13",
        modifier: "#5c7000",
        literal: "#a7440b",
        type_fg: "#7736b6",
        builtin: "#00717b",
        interface: "#21609e",
        generic: "#a43576",
        function: "#00717b",
        property: "#b02c63",
        parameter: "#836400",
        variable: "#1858c0",
        constant: "#b52e31",
        namespace: "#6747a3",
        string: "#3d7319",
        number: "#a56300",
        macro_fg: "#6547bf",
        preprocessor: None,
    },
    Palette {
        surface: "#081629",
        block_surface: "#0e2036",
        text: "#d6ddea",
        dim: "#a1adc1",
        warning: "#ebc376",
        error: "#ff9eac",
        keyword: "#ff88ce",
        flow: "#ffa26e",
        modifier: "#cde46a",
        literal: "#ffae75",
        type_fg: "#e1b8ff",
        builtin: "#61e0df",
        interface: "#8fbfff",
        generic: "#f49dcb",
        function: "#57dce7",
        property: "#ffa0cb",
        parameter: "#e6cf83",
        variable: "#77baff",
        constant: "#ff9698",
        namespace: "#c3a5ff",
        string: "#b4e77c",
        number: "#f7d471",
        macro_fg: "#b8a2ff",
        preprocessor: None,
    },
];

const LILAC: [Palette; 2] = [
    Palette {
        surface: "#fdfbff",
        block_surface: "#f5eff9",
        text: "#382e42",
        dim: "#72647c",
        warning: "#986300",
        error: "#b52e3d",
        keyword: "#6432ad",
        flow: "#ad375a",
        modifier: "#716025",
        literal: "#a83852",
        type_fg: "#9d3c6f",
        builtin: "#4f52a0",
        interface: "#27638a",
        generic: "#876010",
        function: "#006c70",
        property: "#95501c",
        parameter: "#365f83",
        variable: "#77418e",
        constant: "#b13542",
        namespace: "#645b75",
        string: "#5a6c21",
        number: "#a34826",
        macro_fg: "#93499c",
        preprocessor: None,
    },
    Palette {
        surface: "#271c33",
        block_surface: "#30233e",
        text: "#eee2f3",
        dim: "#b3a3bc",
        warning: "#ebc376",
        error: "#ff9eac",
        keyword: "#b89eff",
        flow: "#ff9caa",
        modifier: "#d9c38d",
        literal: "#ff9eb3",
        type_fg: "#e78dc8",
        builtin: "#afbaf6",
        interface: "#88cfe2",
        generic: "#e5cc79",
        function: "#77d6c1",
        property: "#edbd8d",
        parameter: "#9bbfe3",
        variable: "#d4b2ef",
        constant: "#ffa1a7",
        namespace: "#b9aecb",
        string: "#c1d597",
        number: "#ffb58d",
        macro_fg: "#d99be4",
        preprocessor: None,
    },
];

const LILAC_WHITE: [Palette; 2] = [
    Palette {
        surface: "#ffffff",
        block_surface: "#f5f2f8",
        text: "#382e42",
        dim: "#72647c",
        warning: "#986300",
        error: "#b52e3d",
        keyword: "#6432ad",
        flow: "#ad375a",
        modifier: "#716025",
        literal: "#a83852",
        type_fg: "#9d3c6f",
        builtin: "#4f52a0",
        interface: "#27638a",
        generic: "#876010",
        function: "#006c70",
        property: "#95501c",
        parameter: "#365f83",
        variable: "#77418e",
        constant: "#b13542",
        namespace: "#645b75",
        string: "#5a6c21",
        number: "#a34826",
        macro_fg: "#93499c",
        preprocessor: None,
    },
    Palette {
        surface: "#271c33",
        block_surface: "#30233e",
        text: "#eee2f3",
        dim: "#b3a3bc",
        warning: "#ebc376",
        error: "#ff9eac",
        keyword: "#b89eff",
        flow: "#ff9caa",
        modifier: "#d9c38d",
        literal: "#ff9eb3",
        type_fg: "#e78dc8",
        builtin: "#afbaf6",
        interface: "#88cfe2",
        generic: "#e5cc79",
        function: "#77d6c1",
        property: "#edbd8d",
        parameter: "#9bbfe3",
        variable: "#d4b2ef",
        constant: "#ffa1a7",
        namespace: "#b9aecb",
        string: "#c1d597",
        number: "#ffb58d",
        macro_fg: "#d99be4",
        preprocessor: None,
    },
];

const BALANCED: [Palette; 2] = [
    Palette {
        surface: "#ffffff",
        block_surface: "#f3f5f8",
        text: "#253145",
        dim: "#596879",
        warning: "#986300",
        error: "#b52e3d",
        keyword: "#245a99",
        flow: "#923f35",
        modifier: "#7a5f16",
        literal: "#a34c16",
        type_fg: "#194800",
        builtin: "#006f73",
        interface: "#006e86",
        generic: "#a43576",
        function: "#805600",
        property: "#74499d",
        parameter: "#963e65",
        variable: "#28704a",
        constant: "#9d4923",
        namespace: "#375f9b",
        string: "#4e6e24",
        number: "#8e3f87",
        macro_fg: "#894f84",
        preprocessor: None,
    },
    Palette {
        surface: "#141518",
        block_surface: "#1e2025",
        text: "#d5dce7",
        dim: "#a1adc1",
        warning: "#ebc376",
        error: "#ff9eac",
        keyword: "#9cbbef",
        flow: "#e8a59b",
        modifier: "#dac18b",
        literal: "#f5ac78",
        type_fg: "#b9da61",
        builtin: "#86cec8",
        interface: "#83c9dc",
        generic: "#f49dcb",
        function: "#e0bf85",
        property: "#c0a8e0",
        parameter: "#dda2bd",
        variable: "#91cfac",
        constant: "#e4af90",
        namespace: "#a2b9dd",
        string: "#afce88",
        number: "#d9a3d6",
        macro_fg: "#d1acd3",
        preprocessor: None,
    },
];

const PAPER: [Palette; 2] = [
    Palette {
        surface: "#fbf7ef",
        block_surface: "#f2ebdf",
        text: "#342e28",
        dim: "#716859",
        warning: "#986300",
        error: "#b52e3d",
        keyword: "#8a3b73",
        flow: "#b03926",
        modifier: "#657123",
        literal: "#a83e22",
        type_fg: "#574000",
        builtin: "#68519e",
        interface: "#2e6798",
        generic: "#a45d10",
        function: "#386b2b",
        property: "#285d9a",
        parameter: "#006e6a",
        variable: "#8f4b2c",
        constant: "#aa322e",
        namespace: "#6b5275",
        string: "#806014",
        number: "#9b2c77",
        macro_fg: "#3c6d69",
        preprocessor: None,
    },
    Palette {
        surface: "#211e1a",
        block_surface: "#2b2721",
        text: "#e6dfd3",
        dim: "#b6aa96",
        warning: "#ebc376",
        error: "#ff9eac",
        keyword: "#d9a0c4",
        flow: "#f3a38f",
        modifier: "#c6d48c",
        literal: "#efa18c",
        type_fg: "#f0d79a",
        builtin: "#c4afe2",
        interface: "#9dc1e9",
        generic: "#e9bd7c",
        function: "#afd499",
        property: "#99bee9",
        parameter: "#88d1c5",
        variable: "#e4b28d",
        constant: "#eea29e",
        namespace: "#c0accb",
        string: "#d8c179",
        number: "#e5a3cc",
        macro_fg: "#9ccac2",
        preprocessor: None,
    },
];

fn palette(theme: CodeTheme, mode: ThemeMode) -> &'static Palette {
    let pair = match theme {
        CodeTheme::Prism => &PRISM,
        CodeTheme::Orchard => &ORCHARD,
        CodeTheme::Spectrum => &SPECTRUM,
        CodeTheme::Neon => &NEON,
        CodeTheme::Lilac => &LILAC,
        CodeTheme::LilacWhite => &LILAC_WHITE,
        CodeTheme::Balanced => &BALANCED,
        CodeTheme::Paper => &PAPER,
    };
    match mode {
        ThemeMode::Light => &pair[0],
        ThemeMode::Dark => &pair[1],
    }
}

fn theme_name(theme: CodeTheme) -> &'static str {
    match theme {
        CodeTheme::Prism => "Prism",
        CodeTheme::Orchard => "Orchard",
        CodeTheme::Spectrum => "Spectrum",
        CodeTheme::Neon => "Neon",
        CodeTheme::Lilac => "Lilac",
        CodeTheme::LilacWhite => "Lilac White",
        CodeTheme::Balanced => "Balanced",
        CodeTheme::Paper => "Paper",
    }
}

pub(crate) fn surface(theme: CodeTheme, mode: ThemeMode) -> Hsla {
    hsla(palette(theme, mode).surface)
}

/// Generate a syntax theme for code surfaces.
///
/// `app_backgrounds` contains the app's main, elevated, and muted backgrounds. Adjust contrast for these
/// backgrounds as well so code remains readable when a code block uses an app background.
pub(crate) fn highlight_theme(
    theme: CodeTheme,
    contrast: CodeContrast,
    mode: ThemeMode,
    app_backgrounds: &[Hsla],
) -> Arc<HighlightTheme> {
    let palette = palette(theme, mode);
    let mut backgrounds = vec![
        palette.surface.to_string(),
        palette.block_surface.to_string(),
    ];
    backgrounds.extend(app_backgrounds.iter().copied().map(hsla_hex));
    if mode == ThemeMode::Light {
        backgrounds.push("#f5f5f5".into());
    }
    let focus = hsla(palette.surface).blend(
        hsla(if mode.is_dark() { "#8ea2ff" } else { "#4f6bed" }).opacity(if mode.is_dark() {
            0.16
        } else {
            0.12
        }),
    );
    let clear = !mode.is_dark() && matches!(theme, CodeTheme::Prism | CodeTheme::Spectrum);
    if clear {
        backgrounds.push(hsla_hex(focus));
    }
    let mut colors = colors(theme, contrast, mode, &backgrounds);
    if !clear {
        backgrounds.push(hsla_hex(focus));
        let luminances = backgrounds
            .iter()
            .map(|color| luminance(color))
            .collect::<Vec<_>>();
        colors.comment = comment_color(mode.is_dark(), contrast_target(contrast), &luminances);
    }
    build_highlight_theme(theme, mode, &colors)
}

/// All contrast-adjusted colors, used directly by settings previews and tests.
struct Colors {
    surface: String,
    block_surface: String,
    text: String,
    dim: String,
    warning: String,
    error: String,
    keyword: String,
    flow: String,
    modifier: String,
    literal: String,
    type_fg: String,
    builtin: String,
    interface: String,
    generic: String,
    function: String,
    property: String,
    parameter: String,
    variable: String,
    constant: String,
    namespace: String,
    string: String,
    number: String,
    macro_fg: String,
    preprocessor: String,
    comment: String,
}

fn colors(
    theme: CodeTheme,
    contrast: CodeContrast,
    mode: ThemeMode,
    backgrounds: &[String],
) -> Colors {
    let palette = palette(theme, mode);
    let target = contrast_target(contrast);
    let dark = mode == ThemeMode::Dark;
    let surface = palette.surface.to_string();
    let block_surface = palette.block_surface.to_string();
    // Like the web `codePalette`, check only the code background when no other backgrounds are supplied.
    let on = if backgrounds.is_empty() {
        vec![surface.clone(), block_surface.clone()]
    } else {
        backgrounds.to_vec()
    };
    let luminances: Vec<f64> = on.iter().map(|color| luminance(color)).collect();
    // Light prism and spectrum retain higher chroma. Reduce chroma to fit the gamut before conversion.
    let clear = !dark && matches!(theme, CodeTheme::Prism | CodeTheme::Spectrum);
    let chroma = if clear { 1.07 } else { 1.15 };
    let fit = if clear {
        fit_chroma as fn(f64, f64, f64) -> String
    } else {
        in_gamut as fn(f64, f64, f64) -> String
    };
    let mut memo: HashMap<String, String> = HashMap::new();
    let mut color = |value: &str| -> String {
        if let Some(color) = memo.get(value) {
            return color.clone();
        }
        let color = readable(value, &luminances, target, dark, chroma, fit);
        memo.insert(value.to_string(), color.clone());
        color
    };
    Colors {
        text: color(palette.text),
        dim: color(palette.dim),
        warning: color(palette.warning),
        error: color(palette.error),
        keyword: color(palette.keyword),
        flow: color(palette.flow),
        modifier: color(palette.modifier),
        literal: color(palette.literal),
        type_fg: color(palette.type_fg),
        builtin: color(palette.builtin),
        interface: color(palette.interface),
        generic: color(palette.generic),
        function: color(palette.function),
        property: color(palette.property),
        parameter: color(palette.parameter),
        variable: color(palette.variable),
        constant: color(palette.constant),
        namespace: color(palette.namespace),
        string: color(palette.string),
        number: color(palette.number),
        macro_fg: color(palette.macro_fg),
        preprocessor: color(palette.preprocessor.unwrap_or(palette.macro_fg)),
        comment: comment_color(dark, target, &luminances),
        surface,
        block_surface,
    }
}

fn build_highlight_theme(
    theme: CodeTheme,
    mode: ThemeMode,
    colors: &Colors,
) -> Arc<HighlightTheme> {
    let mut base = (*match mode {
        ThemeMode::Light => HighlightTheme::default_light(),
        ThemeMode::Dark => HighlightTheme::default_dark(),
    })
    .clone();
    base.name = format!(
        "{} {}",
        theme_name(theme),
        match mode {
            ThemeMode::Light => "Light",
            ThemeMode::Dark => "Dark",
        }
    );
    base.appearance = mode;
    base.style = serde_json::from_value::<HighlightThemeStyle>(style(colors, mode))
        .expect("code theme style");
    Arc::new(base)
}

fn syntax_style(colors: &Colors, semantics: bool) -> Value {
    let color = |value: &str| json!({ "color": value });
    let mut syntax = Map::new();
    for (capture, value) in [
        ("attribute", &colors.macro_fg),
        ("boolean", &colors.literal),
        ("comment", &colors.comment),
        ("comment.doc", &colors.comment),
        ("constant", &colors.constant),
        ("constructor", &colors.number),
        ("embedded", &colors.text),
        ("enum", &colors.type_fg),
        ("function", &colors.function),
        ("hint", &colors.dim),
        ("keyword", &colors.keyword),
        ("label", &colors.generic),
        ("number", &colors.number),
        ("operator", &colors.text),
        ("predictive", &colors.dim),
        ("preproc", &colors.preprocessor),
        ("primary", &colors.text),
        ("property", &colors.property),
        ("punctuation", &colors.text),
        ("punctuation.bracket", &colors.text),
        ("punctuation.delimiter", &colors.text),
        ("punctuation.list_marker", &colors.dim),
        ("punctuation.special", &colors.text),
        ("string", &colors.string),
        ("string.escape", &colors.string),
        ("string.regex", &colors.string),
        ("string.special", &colors.string),
        ("string.special.symbol", &colors.string),
        ("tag", &colors.type_fg),
        ("tag.doctype", &colors.preprocessor),
        ("text.code.span", &colors.literal),
        ("text.literal", &colors.text),
        ("title", &colors.function),
        ("type", &colors.type_fg),
        ("variable", &colors.variable),
        ("variable.special", &colors.parameter),
        ("variant", &colors.number),
    ] {
        syntax.insert(capture.into(), color(value));
    }
    syntax.insert("emphasis".into(), json!({ "font_style": "italic" }));
    syntax.insert("emphasis.strong".into(), json!({ "font_weight": 700 }));
    syntax.insert(
        "link_text".into(),
        json!({ "color": colors.function, "font_style": "normal" }),
    );
    syntax.insert(
        "link_uri".into(),
        json!({ "color": colors.dim, "font_style": "italic" }),
    );

    for name in ["comment", "comment.doc"] {
        syntax.insert(
            name.into(),
            json!({ "color": colors.comment, "font_style": "italic" }),
        );
    }

    let mut additional = Map::new();
    for name in ["conditional", "coroutine", "exception", "repeat", "return"] {
        additional.insert(format!("keyword.{name}"), color(&colors.flow));
    }
    for (name, value) in [
        ("punctuation.delimiter.code", &colors.dim),
        ("keyword.modifier", &colors.modifier),
        ("type.builtin", &colors.builtin),
        ("type.interface", &colors.interface),
        ("type.parameter", &colors.generic),
        ("module", &colors.namespace),
        ("namespace", &colors.namespace),
        ("variable.parameter", &colors.parameter),
        ("variable.member", &colors.property),
        ("function.macro", &colors.macro_fg),
        ("constant.builtin", &colors.literal),
        ("constant.builtin.boolean", &colors.literal),
        ("constant.builtin.number", &colors.number),
        ("escape", &colors.string),
    ] {
        additional.insert(name.into(), color(value));
    }
    if semantics {
        let surface = hsla(&colors.surface);
        let active_line =
            surface.blend(hsla(&colors.text).opacity(f32::from(ACTIVE_LINE_ALPHA) / 255.));
        let underline_backgrounds = [surface, active_line];
        let method_underline = underline_color(&colors.function, underline_backgrounds);
        let property_underline = underline_color(&colors.property, underline_backgrounds);
        for &role in crate::lsp::SEMANTIC_ROLES {
            let foreground = match role {
                "namespace" => &colors.namespace,
                "type" | "enum" => &colors.type_fg,
                "type.builtin" => &colors.builtin,
                "type.interface" => &colors.interface,
                "type.parameter" | "label" => &colors.generic,
                "function" | "function.method" => &colors.function,
                "function.macro" => &colors.preprocessor,
                "property" => &colors.property,
                "constructor" | "variant" => &colors.number,
                "variable.special" => &colors.parameter,
                "variable" => &colors.variable,
                "constant" => &colors.constant,
                "attribute" => &colors.macro_fg,
                "keyword.modifier" => &colors.modifier,
                "boolean" => &colors.literal,
                "string" | "string.regex" => &colors.string,
                "number" => &colors.number,
                "comment" | "comment.doc" => &colors.comment,
                "operator" | "punctuation" => &colors.text,
                // Keep the grammar's flow, modifier and literal colors for generic LSP keywords.
                "keyword" | "keyword.self" => &colors.keyword,
                _ => unreachable!("unknown semantic role {role}"),
            };
            let underline = match role {
                "function.method" => Some((method_underline.as_str(), "dashed")),
                "property" => Some((property_underline.as_str(), "dotted")),
                _ => None,
            };
            for flags in 0..crate::lsp::SEMANTIC_VARIANTS {
                let (key, style) = semantic_style(role, flags, foreground, underline);
                additional.insert(key, style);
            }
        }
    }
    syntax.insert("additional".into(), Value::Object(additional));
    Value::Object(syntax)
}

/// Use the Web diff accents with a softer added-row fill on the active code surface.
pub(crate) fn apply_diff_contrast(
    highlight: &mut HighlightTheme,
    theme: CodeTheme,
    contrast: CodeContrast,
    mode: ThemeMode,
) {
    use gpui_component::highlighter::TintedSyntax;

    let [removed, added] = match mode {
        ThemeMode::Light => [["#fcebe9", "#f6cdc7"], ["#f5faf6", "#dfefe3"]],
        ThemeMode::Dark => [["#2c1c1c", "#552b26"], ["#1b211e", "#294132"]],
    };
    let surface = hsla(palette(theme, mode).surface);
    let added_row = hsla_hex(surface.blend(hsla(added[0]).opacity(0.6)));
    let styles = |backgrounds: [&str; 2]| {
        let [row, inline] = backgrounds.map(|background| {
            let colors = colors(theme, contrast, mode, &[background.into()]);
            TintedSyntax {
                background: hsla(background),
                syntax: serde_json::from_value(syntax_style(&colors, false)).expect("diff syntax"),
            }
        });
        (row, inline)
    };
    highlight.style.diff_removed = Some(styles(removed));
    highlight.style.diff_added = Some(styles([&added_row, added[1]]));
}

fn style(colors: &Colors, mode: ThemeMode) -> Value {
    let mut style = Map::new();
    style.insert("code_block.background".into(), json!(colors.block_surface));
    style.insert("editor.background".into(), json!(colors.surface.clone()));
    style.insert("editor.foreground".into(), json!(colors.text.clone()));
    // Blend in 6% foreground for the current line to keep diff backgrounds visible.
    style.insert(
        "editor.active_line.background".into(),
        json!(format!("{}{:02x}", colors.text, ACTIVE_LINE_ALPHA)),
    );
    style.insert("editor.line_number".into(), json!(colors.dim.clone()));
    style.insert(
        "editor.active_line_number".into(),
        json!(colors.text.clone()),
    );
    style.insert(
        "editor.invisible".into(),
        json!(format!("{}66", colors.dim)),
    );
    style.insert(
        "editor.gutter.background".into(),
        json!(colors.surface.clone()),
    );
    style.insert("warning".into(), json!(colors.warning.clone()));
    style.insert("error".into(), json!(colors.error.clone()));
    let (info, hint) = match mode {
        ThemeMode::Light => ("#697386", "#4f6bed"),
        ThemeMode::Dark => ("#969eae", "#8ea2ff"),
    };
    style.insert(
        "editor.search_match.background".into(),
        json!(if mode.is_dark() { "#6b5a1d" } else { "#ffe9a8" }),
    );
    style.insert("info".into(), json!(info));
    style.insert("hint".into(), json!(hint));
    style.insert("syntax".into(), syntax_style(colors, true));
    Value::Object(style)
}

fn semantic_style(
    role: &str,
    flags: u32,
    foreground: &str,
    underline: Option<(&str, &str)>,
) -> (String, Value) {
    use crate::lsp::{DECLARATION, DEPRECATED, READONLY, semantic_style_key};

    let declaration = flags & DECLARATION != 0;
    let deprecated = flags & DEPRECATED != 0;
    let weight = match role {
        "function.macro" => 600,
        "type" | "enum" | "type.interface" | "function" | "function.method" if declaration => 500,
        _ => 400,
    };
    let italic = matches!(role, "comment" | "comment.doc" | "type.parameter" | "label");
    let mut style = json!({
        "font_weight": weight,
        "font_style": if italic { "italic" } else { "normal" },
    });
    // Clangd also marks the punctuation of overloaded operator names as
    // operators. Keep the more specific grammar color for the whole name.
    if role != "keyword" && !(role == "operator" && declaration) {
        style["color"] = json!(foreground);
    }
    if deprecated {
        style["strikethrough"] = json!({ "thickness": 1.0 });
    } else if let Some((color, pattern)) = underline
        && (role == "function.method" || (declaration && flags & READONLY != 0))
    {
        style["underline"] = json!({
            "color": color, "thickness": 1.0, "wavy": false, "pattern": pattern,
        });
    }
    (semantic_style_key(role, flags), style)
}

/// Blend toward the token color just far enough to be visible on both editor backgrounds.
fn underline_color(foreground: &str, backgrounds: [Hsla; 2]) -> String {
    let color = hsla(foreground);
    let [surface, _] = backgrounds;
    let luminances = backgrounds.map(|background| luminance(&hsla_hex(background)));
    let (mut low, mut high) = (0u8, 255u8);
    // Resolve the tint to an opaque color so the underline pipeline does not have to blend it.
    let blend = |alpha| hsla_hex(surface.blend(color.opacity(f32::from(alpha) / 255.)));
    while low < high {
        let alpha = low + (high - low) / 2;
        let candidate = luminance(&blend(alpha));
        if luminances
            .iter()
            .all(|&background| contrast_ratio(candidate, background) >= SEMANTIC_UNDERLINE_CONTRAST)
        {
            high = alpha;
        } else {
            low = alpha + 1;
        }
    }
    blend(low)
}

fn hsla(hex: &str) -> Hsla {
    let value = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or_default();
    gpui::rgba((value << 8) | 0xff).into()
}

fn hsla_hex(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(rgba.r),
        channel(rgba.g),
        channel(rgba.b)
    )
}

fn channels(color: &str) -> [f64; 3] {
    let hex = color.trim_start_matches('#');
    std::array::from_fn(|index| {
        u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap_or_default() as f64
    })
}

fn to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn from_linear(value: f64) -> f64 {
    if value <= 0.0031308 {
        12.92 * value
    } else {
        1.055 * value.max(0.).powf(1. / 2.4) - 0.055
    }
}

fn hex(values: [f64; 3]) -> String {
    let mut color = String::from("#");
    for value in values {
        color.push_str(&format!(
            "{:02x}",
            (value.clamp(0., 1.) * 255.).round() as u8
        ));
    }
    color
}

fn luminance(color: &str) -> f64 {
    let [red, green, blue] = channels(color).map(|value| to_linear(value / 255.));
    red * 0.2126 + green * 0.7152 + blue * 0.0722
}

fn contrast_ratio(first: f64, second: f64) -> f64 {
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

fn oklab(color: &str) -> [f64; 3] {
    let [red, green, blue] = channels(color).map(|value| to_linear(value / 255.));
    let l = (0.4122214708 * red + 0.5363325363 * green + 0.0514459929 * blue).cbrt();
    let m = (0.2119034982 * red + 0.6806995451 * green + 0.1073969566 * blue).cbrt();
    let s = (0.0883024619 * red + 0.2817188376 * green + 0.6299787005 * blue).cbrt();
    [
        0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
    ]
}

fn from_oklab(lightness: f64, a: f64, b: f64) -> [f64; 3] {
    let l = (lightness + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let m = (lightness - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let s = (lightness - 0.0894841775 * a - 1.291485548 * b).powi(3);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
    ]
    .map(from_linear)
}

fn in_gamut(lightness: f64, a: f64, b: f64) -> String {
    let (mut a, mut b) = (a, b);
    for _ in 0..60 {
        let channels = from_oklab(lightness, a, b);
        if channels.iter().all(|value| (0. ..=1.).contains(value)) {
            return hex(channels);
        }
        a *= 0.94;
        b *= 0.94;
    }
    hex(from_oklab(lightness, 0., 0.))
}

fn fit_chroma(lightness: f64, a: f64, b: f64) -> String {
    let full = from_oklab(lightness, a, b);
    if full.iter().all(|value| (0. ..=1.).contains(value)) {
        return hex(full);
    }
    let (mut low, mut high) = (0., 1.);
    for _ in 0..16 {
        let factor = (low + high) / 2.;
        let channels = from_oklab(lightness, a * factor, b * factor);
        if channels.iter().all(|value| (0. ..=1.).contains(value)) {
            low = factor;
        } else {
            high = factor;
        }
    }
    hex(from_oklab(lightness, a * low, b * low))
}

/// Adjust foreground lightness until contrast against every background reaches the target.
///
/// If even white or black falls short (for example, at high contrast on a midtone background),
/// use the highest achievable contrast.
fn readable(
    color: &str,
    backgrounds: &[f64],
    target: f64,
    dark: bool,
    chroma: f64,
    fit: fn(f64, f64, f64) -> String,
) -> String {
    let end = if dark { "#ffffff" } else { "#000000" };
    let end_luminance = luminance(end);
    let floor = backgrounds.iter().fold(target, |floor, background| {
        floor.min(contrast_ratio(end_luminance, *background))
    });
    let [lightness, a, b] = oklab(color);
    let finish = if dark { 1. } else { 0. };
    for step in 0..=300 {
        let candidate = fit(
            lightness + (finish - lightness) * f64::from(step) / 300.,
            a * chroma,
            b * chroma,
        );
        let candidate_luminance = luminance(&candidate);
        if backgrounds
            .iter()
            .all(|background| contrast_ratio(candidate_luminance, *background) + 1e-9 >= floor)
        {
            return candidate;
        }
    }
    end.to_string()
}

fn comment_color(dark: bool, target: f64, backgrounds: &[f64]) -> String {
    let gray = if dark { "#c5c5c5" } else { "#919191" };
    readable(gray, backgrounds, target, dark, 1., in_gamut)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [CodeTheme; 8] = [
        CodeTheme::Prism,
        CodeTheme::Orchard,
        CodeTheme::Spectrum,
        CodeTheme::Neon,
        CodeTheme::Lilac,
        CodeTheme::LilacWhite,
        CodeTheme::Balanced,
        CodeTheme::Paper,
    ];

    fn app_backgrounds(mode: ThemeMode) -> Vec<String> {
        // Match the backgrounds from `Palette::for_mode`.
        match mode {
            ThemeMode::Light => vec!["#ffffff".into(), "#ffffff".into(), "#f5f6f9".into()],
            ThemeMode::Dark => vec!["#141518".into(), "#1c1d22".into(), "#232428".into()],
        }
    }

    fn ordinary(theme: CodeTheme, mode: ThemeMode) -> Vec<String> {
        let palette = palette(theme, mode);
        let mut backgrounds = vec![
            palette.surface.to_string(),
            palette.block_surface.to_string(),
        ];
        backgrounds.extend(app_backgrounds(mode));
        backgrounds
    }

    fn all_foregrounds(colors: &Colors) -> Vec<&str> {
        vec![
            &colors.text,
            &colors.dim,
            &colors.warning,
            &colors.error,
            &colors.keyword,
            &colors.literal,
            &colors.type_fg,
            &colors.function,
            &colors.property,
            &colors.parameter,
            &colors.variable,
            &colors.constant,
            &colors.string,
            &colors.number,
            &colors.macro_fg,
            &colors.preprocessor,
            &colors.comment,
        ]
    }

    #[test]
    fn enhanced_preview_colors_match_the_web_version() {
        let preview = ["#ffffff", "#f3f5f8", "#f5f6f9", "#f5f5f5", "#eaedfd"].map(String::from);
        for (theme, mode, backgrounds) in [
            (CodeTheme::Prism, ThemeMode::Light, preview.as_slice()),
            (CodeTheme::Spectrum, ThemeMode::Light, preview.as_slice()),
            (CodeTheme::Spectrum, ThemeMode::Dark, &[][..]),
        ] {
            let colors = colors(theme, CodeContrast::Enhanced, mode, backgrounds);
            let expected: [&str; 5] = match (theme, mode) {
                (CodeTheme::Prism, ThemeMode::Light) => {
                    ["#b40033", "#0050ca", "#924200", "#7500e0", "#66500a"]
                }
                (CodeTheme::Spectrum, ThemeMode::Light) => {
                    ["#b40032", "#28384c", "#944000", "#162ab6", "#a42922"]
                }
                (CodeTheme::Spectrum, ThemeMode::Dark) => {
                    ["#fba664", "#c6cdda", "#e6ce74", "#93b3fc", "#fdae91"]
                }
                _ => unreachable!(),
            };
            assert_eq!(
                [
                    colors.literal.as_str(),
                    colors.variable.as_str(),
                    colors.parameter.as_str(),
                    colors.function.as_str(),
                    colors.constant.as_str(),
                ],
                expected,
                "{theme:?} {mode:?}"
            );
        }

        let prism = colors(
            CodeTheme::Prism,
            CodeContrast::Enhanced,
            ThemeMode::Light,
            &preview,
        );
        assert_eq!(prism.macro_fg, "#7500e0");
        assert_eq!(prism.preprocessor, "#005b64");
        assert_eq!(prism.text, "#24242b");
        assert_eq!(prism.comment, "#595959");
    }

    #[test]
    fn every_foreground_meets_its_contrast_target() {
        for theme in ALL {
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                for contrast in [
                    CodeContrast::Standard,
                    CodeContrast::Enhanced,
                    CodeContrast::High,
                ] {
                    let backgrounds = ordinary(theme, mode);
                    let colors = colors(theme, contrast, mode, &backgrounds);
                    let target = contrast_target(contrast);
                    let end_luminance = luminance(if mode == ThemeMode::Dark {
                        "#ffffff"
                    } else {
                        "#000000"
                    });
                    for background in &backgrounds {
                        let background = luminance(background);
                        let floor = target.min(contrast_ratio(end_luminance, background));
                        for foreground in all_foregrounds(&colors) {
                            let ratio = contrast_ratio(luminance(foreground), background);
                            assert!(
                                ratio + 1e-9 >= floor,
                                "{theme:?} {mode:?} {contrast:?}: {foreground} on {background} is {ratio:.2}, wanted {floor:.2}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_theme_builds_a_parseable_highlight_theme() {
        for theme in ALL {
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                let backgrounds = app_backgrounds(mode)
                    .iter()
                    .map(|background| hsla(background))
                    .collect::<Vec<_>>();
                for contrast in [
                    CodeContrast::Standard,
                    CodeContrast::Enhanced,
                    CodeContrast::High,
                ] {
                    let highlight = highlight_theme(theme, contrast, mode, &backgrounds);
                    assert_eq!(highlight.appearance, mode);
                    assert!(highlight.style.editor_background.is_some());
                    for capture in ["keyword", "string", "comment", "function", "type"] {
                        assert!(
                            highlight.style(capture).is_some(),
                            "{theme:?} {mode:?} {contrast:?}: {capture}"
                        );
                    }
                    let surface = highlight.style.editor_background.unwrap();
                    let active_line = surface.blend(highlight.style.editor_active_line.unwrap());
                    for capture in ["function.method.lsp.0", "property.lsp.3"] {
                        let underline = highlight.style(capture).unwrap().underline.unwrap();
                        let color = underline.color.unwrap();
                        for background in [surface, active_line] {
                            let ratio = contrast_ratio(
                                luminance(&hsla_hex(background.blend(color))),
                                luminance(&hsla_hex(background)),
                            );
                            assert!(
                                (3. ..=3.8).contains(&ratio),
                                "{theme:?} {mode:?} {contrast:?}: {capture} has {ratio:.2}:1 contrast"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn surface_matches_the_palette() {
        assert_eq!(surface(CodeTheme::Prism, ThemeMode::Light), hsla("#ffffff"));
        assert_eq!(surface(CodeTheme::Neon, ThemeMode::Dark), hsla("#081629"));
        assert_eq!(surface(CodeTheme::Paper, ThemeMode::Light), hsla("#fbf7ef"));
    }
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn cpp_highlights_survive_delayed_semantic_responses() -> anyhow::Result<()> {
    use std::{cell::RefCell, rc::Rc, time::Duration};

    use futures_channel::oneshot;
    use gpui::{
        AppContext as _, Bounds, DevicePixels, Entity, HeadlessAppContext, Hsla, PlatformAtlas,
        PlatformHeadlessRenderer, Render, ScaledPixels, Scene, Size, Styled as _, Task, Window, px,
        size,
    };
    use gpui_component::{
        ActiveTheme as _, Root,
        input::{DocumentRangeSemanticTokensProvider, Editor, EditorState, Rope},
    };
    use gpui_wgpu::WgpuHeadlessRenderer;
    use lsp_types::{SemanticToken, SemanticTokens, SemanticTokensLegend};

    const SOURCE: &str = concat!(
        "class VectorPath {\n",
        "    GX_NODISCARD bool isEmpty() const;\n",
        "    GX_NODISCARD constexpr const Vertex &operator[](unsigned i) const;\n",
        "    bool operator==(const VectorPath &other) const;\n",
        "};\n",
    );
    type Reply = oneshot::Sender<SemanticTokens>;
    #[derive(Default)]
    struct Tokens(RefCell<Vec<(String, Reply)>>);
    impl DocumentRangeSemanticTokensProvider for Tokens {
        fn legend(&self) -> SemanticTokensLegend {
            SemanticTokensLegend {
                token_types: ["property.lsp.0", "operator.lsp.1", "constant.lsp.0"]
                    .map(Into::into)
                    .to_vec(),
                ..Default::default()
            }
        }

        fn semantic_tokens(
            &self,
            text: &Rope,
            _: std::ops::Range<usize>,
            _: &mut Window,
            cx: &mut gpui::App,
        ) -> Task<anyhow::Result<SemanticTokens>> {
            let (reply, response) = oneshot::channel();
            self.0.borrow_mut().push((text.to_string(), reply));
            cx.background_spawn(async move { Ok(response.await?) })
        }
    }

    struct Probe(Entity<EditorState>);
    impl Render for Probe {
        fn render(
            &mut self,
            _: &mut Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            Editor::new(&self.0).text_size(px(17.)).size_full()
        }
    }

    type Glyphs = Rc<RefCell<Vec<(Bounds<ScaledPixels>, Hsla)>>>;
    struct Renderer(WgpuHeadlessRenderer, Glyphs);
    impl PlatformHeadlessRenderer for Renderer {
        fn render_scene(
            &mut self,
            scene: &Scene,
            viewport: Size<DevicePixels>,
        ) -> anyhow::Result<()> {
            self.0.render_scene(scene, viewport)
        }

        fn render_scene_to_image(
            &mut self,
            scene: &Scene,
            viewport: Size<DevicePixels>,
        ) -> anyhow::Result<image::RgbaImage> {
            self.1.replace(
                scene
                    .monochrome_sprites
                    .iter()
                    .map(|glyph| (glyph.bounds, glyph.color))
                    .chain(
                        scene
                            .subpixel_sprites
                            .iter()
                            .map(|glyph| (glyph.bounds, glyph.color)),
                    )
                    .collect(),
            );
            self.0.render_scene_to_image(scene, viewport)
        }

        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.0.sprite_atlas()
        }
    }

    let glyphs = Glyphs::default();
    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::Assets),
        {
            let glyphs = glyphs.clone();
            move || {
                Ok(Some(Box::new(Renderer(
                    WgpuHeadlessRenderer::new()?,
                    glyphs.clone(),
                ))))
            }
        },
    );
    cx.update(|cx| {
        gpui_component::init(cx);
        super::apply_theme(ThemeMode::Light, cx);
    });
    let mut opened = None;
    let window = cx.open_window(size(px(930.), px(230.)), |window, cx| {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("cpp")
                .default_value(SOURCE)
        });
        opened = Some(editor.clone());
        let probe = cx.new(|_| Probe(editor));
        cx.new(|cx| Root::new(probe, window, cx))
    })?;
    let editor = opened.unwrap();
    let assert_color =
        |cx: &mut HeadlessAppContext, needle: &str, color: &str| -> anyhow::Result<()> {
            let (bounds, expected) = cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                let state = editor.read(cx);
                let start = state.text().to_string().find(needle).unwrap();
                (
                    state
                        .range_to_bounds(&(start..start + needle.len()))
                        .unwrap()
                        .scale(window.scale_factor()),
                    cx.theme()
                        .highlight_theme
                        .style(color)
                        .unwrap()
                        .color
                        .unwrap(),
                )
            })?;
            let _ = cx.capture_screenshot(window.into())?;
            let colors = glyphs.borrow();
            let actual = colors
                .iter()
                .filter(|(glyph, _)| bounds.contains(&glyph.center()))
                .map(|(_, color)| *color)
                .collect::<Vec<_>>();
            assert!(!actual.is_empty(), "{needle:?} has no painted glyphs");
            assert!(
                actual.iter().all(|color| *color == expected),
                "{needle:?} {color}: {actual:?}"
            );
            Ok(())
        };
    let frame = |cx: &mut HeadlessAppContext| -> anyhow::Result<()> {
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
        cx.run_until_parked();
        cx.advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        Ok(())
    };
    let response = |line, kind| SemanticTokens {
        data: [
            SemanticToken {
                delta_line: line,
                delta_start: SOURCE.lines().nth(1).unwrap().find("isEmpty").unwrap() as u32,
                length: 7,
                token_type: kind,
                token_modifiers_bitset: 0,
            },
            SemanticToken {
                delta_line: 1,
                delta_start: SOURCE.lines().nth(2).unwrap().find('[').unwrap() as u32,
                length: 1,
                token_type: 1,
                token_modifiers_bitset: 0,
            },
            SemanticToken {
                delta_line: 0,
                delta_start: 1,
                length: 1,
                token_type: 1,
                token_modifiers_bitset: 0,
            },
        ]
        .into(),
        ..Default::default()
    };

    frame(&mut cx)?;
    assert_color(&mut cx, "bool", "type")?;
    assert_color(&mut cx, "operator[]", "function")?;
    assert_color(&mut cx, "operator==", "function")?;
    let provider = Rc::new(Tokens::default());
    cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            editor.lsp_mut().semantic_tokens_provider = Some(provider.clone());
            editor.refresh(cx);
        })
    });
    frame(&mut cx)?;
    let (snapshot, request) = provider.0.borrow_mut().pop().unwrap();
    assert_eq!(snapshot, SOURCE);
    request.send(response(1, 0)).unwrap();
    cx.run_until_parked();
    assert_color(&mut cx, "isEmpty", "property.lsp.0")?;
    assert_color(&mut cx, "operator[]", "function")?;
    assert_color(&mut cx, "[]", "function")?;

    cx.update_window(window.into(), |_, window, cx| {
        editor.update(cx, |editor, cx| {
            editor.apply_editor_edits(&[(0..0, "\n".into())], false, window, cx);
        })
    })?;
    frame(&mut cx)?;
    let (snapshot, delayed) = provider.0.borrow_mut().pop().unwrap();
    assert_eq!(snapshot, format!("\n{SOURCE}"));
    assert_color(&mut cx, "isEmpty", "property.lsp.0")?;

    cx.update_window(window.into(), |_, window, cx| {
        editor.update(cx, |editor, cx| {
            editor.replay_text_history(true, window, cx);
        })
    })?;
    _ = delayed.send(response(2, 2));
    frame(&mut cx)?;
    let (snapshot, current) = provider.0.borrow_mut().pop().unwrap();
    assert_eq!(snapshot, SOURCE);
    assert_color(&mut cx, "isEmpty", "property.lsp.0")?;
    current.send(response(1, 2)).unwrap();
    cx.run_until_parked();
    assert_color(&mut cx, "isEmpty", "constant.lsp.0")?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn editor_renders_web_theme_decorations_in_light_and_dark() -> anyhow::Result<()> {
    use std::{cell::RefCell, rc::Rc, time::Duration};

    use gpui::{
        AppContext as _, DevicePixels, Entity, HeadlessAppContext, ParentElement as _,
        PlatformAtlas, PlatformHeadlessRenderer, Render, Scene, Size, Styled as _, Task, Underline,
        div, px, size,
    };
    use gpui_component::{
        ActiveTheme as _, Root,
        input::{DocumentRangeSemanticTokensProvider, Editor, EditorState, InlayHint, Rope},
    };
    use gpui_wgpu::WgpuHeadlessRenderer;
    use lsp_types::{
        Diagnostic, DiagnosticSeverity, Position, Range, SemanticToken, SemanticTokenType,
        SemanticTokens, SemanticTokensLegend,
    };

    use crate::lsp::{DECLARATION, DEPRECATED, READONLY, semantic_style_key};

    const TEXT: &str = concat!(
        "macro_call!();\n",
        "// Comments use italic type.\n",
        "service.fetch_latest_value();\n",
        "readonly_property: string;\n",
        "old_api();\n",
        "service.method_with_error();\n",
        "let count = 3;\n",
    );
    struct Tokens;
    impl DocumentRangeSemanticTokensProvider for Tokens {
        fn legend(&self) -> SemanticTokensLegend {
            SemanticTokensLegend {
                token_types: [
                    ("function.macro", 0),
                    ("function.method", 0),
                    ("property", DECLARATION | READONLY),
                    ("function", DEPRECATED),
                ]
                .map(|(role, flags)| SemanticTokenType::from(semantic_style_key(role, flags)))
                .to_vec(),
                token_modifiers: Vec::new(),
            }
        }

        fn semantic_tokens(
            &self,
            _: &Rope,
            _: std::ops::Range<usize>,
            _: &mut gpui::Window,
            _: &mut gpui::App,
        ) -> Task<anyhow::Result<SemanticTokens>> {
            Task::ready(Ok(SemanticTokens {
                data: [
                    (0, 0, 10, 0),
                    (2, 8, 18, 1),
                    (1, 0, 17, 2),
                    (1, 0, 7, 3),
                    (1, 8, 17, 1),
                ]
                .into_iter()
                .map(
                    |(delta_line, delta_start, length, token_type)| SemanticToken {
                        delta_line,
                        delta_start,
                        length,
                        token_type,
                        token_modifiers_bitset: 0,
                    },
                )
                .collect(),
                ..Default::default()
            }))
        }
    }

    struct EditorProbe(Entity<EditorState>);

    impl Render for EditorProbe {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            let theme = cx.theme();
            let background = theme
                .highlight_theme
                .style
                .editor_background
                .unwrap_or(theme.background);
            div().size_full().bg(background).child(
                Editor::new(&self.0)
                    .appearance(false)
                    .text_size(px(17.))
                    .size_full(),
            )
        }
    }

    struct Renderer {
        wgpu: WgpuHeadlessRenderer,
        marks: Rc<RefCell<Vec<Underline>>>,
    }

    impl PlatformHeadlessRenderer for Renderer {
        fn render_scene_to_image(
            &mut self,
            scene: &Scene,
            viewport: Size<DevicePixels>,
        ) -> anyhow::Result<image::RgbaImage> {
            self.marks.replace(scene.underlines.clone());
            self.wgpu.render_scene_to_image(scene, viewport)
        }

        fn render_scene(
            &mut self,
            scene: &Scene,
            viewport: Size<DevicePixels>,
        ) -> anyhow::Result<()> {
            self.wgpu.render_scene(scene, viewport)
        }

        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.wgpu.sprite_atlas()
        }
    }

    let marks = Rc::new(RefCell::new(Vec::new()));
    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::Assets),
        {
            let marks = marks.clone();
            move || {
                Ok(Some(Box::new(Renderer {
                    wgpu: WgpuHeadlessRenderer::new()?,
                    marks: marks.clone(),
                })))
            }
        },
    );
    cx.update(|cx| {
        gpui_component::init(cx);
        super::apply_theme(ThemeMode::Light, cx);
    });
    let mut opened = None;
    let window = cx.open_window(size(px(440.), px(250.)), |window, cx| {
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("rust")
                .soft_wrap(true)
                .default_value(TEXT);
            state.lsp_mut().semantic_tokens_provider = Some(Rc::new(Tokens));
            state.diagnostics_mut().unwrap().push(Diagnostic {
                range: Range::new(Position::new(5, 8), Position::new(5, 25)),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Test diagnostic".into(),
                ..Default::default()
            });
            state
        });
        opened = Some(editor.clone());
        let probe = cx.new(|_| EditorProbe(editor));
        cx.new(|cx| Root::new(probe, window, cx))
    })?;
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    cx.update(|cx| {
        opened.unwrap().update(cx, |state, cx| {
            state.set_inlay_hints(
                vec![InlayHint {
                    offset: TEXT.find("count").unwrap() + "count".len(),
                    label: ": i32".into(),
                }],
                cx,
            );
        });
    });
    cx.run_until_parked();
    cx.advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    let output = std::env::var_os("STROLL_THEME_SCREENSHOTS").map(std::path::PathBuf::from);
    for (mode, label) in [(ThemeMode::Light, "light"), (ThemeMode::Dark, "dark")] {
        let (dashed, dotted, strike, error) =
            cx.update_window(window.into(), |_, window, cx| {
                super::apply_theme(mode, cx);
                window.draw(cx).clear(cx);
                let theme = &cx.theme().highlight_theme;
                (
                    theme
                        .style("function.method.lsp.0")
                        .unwrap()
                        .underline
                        .unwrap()
                        .color
                        .unwrap(),
                    theme
                        .style("property.lsp.3")
                        .unwrap()
                        .underline
                        .unwrap()
                        .color
                        .unwrap(),
                    theme.style("function.lsp.4").unwrap().color.unwrap(),
                    theme.style.status.error(cx),
                )
            })?;
        let screenshot = cx.capture_screenshot(window.into())?;
        let underlines = marks.borrow();
        let dashed_marks = underlines
            .iter()
            .filter(|mark| mark.color == dashed)
            .collect::<Vec<_>>();
        let dotted_marks = underlines
            .iter()
            .filter(|mark| mark.color == dotted)
            .collect::<Vec<_>>();
        assert!(
            dashed_marks.len() > 3 && dotted_marks.len() > 3,
            "{label}: structured underlines"
        );
        assert!(dashed_marks[0].bounds.size.width > dotted_marks[0].bounds.size.width);
        for (name, marks, color) in [
            ("dashed", &dashed_marks, dashed),
            ("dotted", &dotted_marks, dotted),
        ] {
            let expected = u32::from(Rgba::from(color)).to_be_bytes();
            assert!(
                marks.iter().any(|mark| {
                    let center = mark.bounds.center();
                    screenshot
                        .get_pixel_checked(center.x.into(), center.y.into())
                        .is_some_and(|pixel| {
                            pixel
                                .0
                                .into_iter()
                                .zip(expected)
                                .all(|(actual, expected)| actual.abs_diff(expected) <= 1)
                        })
                }),
                "{label}: {name} underline paints its theme color"
            );
        }
        assert!(
            underlines
                .iter()
                .any(|mark| mark.color == strike && mark.wavy == false.into())
        );
        let wave = underlines
            .iter()
            .find(|mark| mark.wavy == true.into() && mark.color == error)
            .expect("error diagnostic paints a wave");
        assert!(
            dashed_marks
                .iter()
                .all(|mark| mark.bounds.top() < wave.bounds.top()),
            "the diagnostic suppresses the method underline"
        );
        if let Some(output) = &output {
            std::fs::create_dir_all(output)?;
            screenshot.save(output.join(format!("code-theme-{label}.png")))?;
        }
    }
    Ok(())
}
