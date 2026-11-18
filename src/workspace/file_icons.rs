use std::path::Path;

use gpui::{Styled as _, px, rgb};
use gpui_component::{Icon, Sizable as _};
use gpui_kit_assets::IconName;

use super::{Palette, audio_preview, icon, language_for};

pub(super) fn file_type_icon(path: &Path, colors: Palette, size: f32) -> Icon {
    // File types use a Material-inspired palette on both workspace themes.
    let tint = |light, dark| rgb(if colors.bg.l < 0.5 { dark } else { light }).into();
    let glyph = |name, light, dark| icon(name, tint(light, dark), size);
    let mark = |data: &[u8], light, dark| {
        Icon::default()
            .data(data)
            .with_size(px(size))
            .text_color(tint(light, dark))
            .flex_shrink_0()
    };

    if path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("AGENTS.md"))
    {
        return glyph(IconName::Bot, 0x0e7067, 0x67d4c3);
    }

    let extension = path.extension().and_then(|extension| extension.to_str());
    match language_for(path) {
        "markdown" => mark(
            include_bytes!("file_icons/markdown.svg"),
            0x1764b5,
            0x64b5f6,
        ),
        "rust" => glyph(IconName::Cog, 0xb84a22, 0xff8a65),
        "typescript" | "tsx" => mark(
            include_bytes!("file_icons/typescript.svg"),
            0x0b638d,
            0x7ac7f4,
        ),
        "c" if extension == Some("c") => {
            mark(include_bytes!("file_icons/c.svg"), 0x215ba2, 0x83b7ff)
        }
        "cpp" if matches!(extension, Some("h" | "H" | "hh" | "hpp" | "hxx" | "h++")) => {
            mark(include_bytes!("file_icons/header.svg"), 0x693b9e, 0xc8a3f6)
        }
        "cpp" if extension != Some("mm") => {
            mark(include_bytes!("file_icons/cpp.svg"), 0x693b9e, 0xc8a3f6)
        }
        "python" => mark(include_bytes!("file_icons/python.svg"), 0x1764a3, 0x78baf4),
        "bash" => glyph(IconName::Terminal, 0x286b37, 0x8ad999),
        "javascript" => mark(
            include_bytes!("file_icons/javascript.svg"),
            0x856000,
            0xf6ca58,
        ),
        "html" => glyph(IconName::Code, 0xa8421c, 0xff986c),
        "css" => glyph(IconName::Hash, 0x087183, 0x71d2e4),
        "cmake" => glyph(IconName::Construction, 0xa84231, 0xff9a81),
        "json" | "jsonc" => glyph(IconName::Braces, 0x926100, 0xf9bd50),
        "toml" => mark(include_bytes!("file_icons/toml.svg"), 0xaa3135, 0xf17b7d),
        "yaml" => glyph(IconName::FileCog, 0xaa3135, 0xf17b7d),
        "text" if crate::image_viewer::is_image(path) => glyph(IconName::Image, 0x117065, 0x58c2b6),
        "text" if audio_preview::is_audio(path) => glyph(IconName::Music, 0xa63349, 0xf17d94),
        "text" => Icon::default()
            .data(include_bytes!("file_icons/text.svg"))
            .with_size(px(size))
            .text_color(colors.muted)
            .flex_shrink_0(),
        _ => icon(IconName::FileCode, colors.muted, size),
    }
}
