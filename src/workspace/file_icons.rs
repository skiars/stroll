use std::path::Path;

use gpui::{Styled as _, px, rgb};
use gpui_component::{Icon, Sizable as _};
use gpui_kit_assets::IconName;

use crate::{
    config::FileAssociations, language_resources::LanguageResources, repository::Repository,
};

use super::{Palette, associated_highlight, audio_preview, icon, language_for};

pub(super) fn file_type_icon(
    repository: Option<&Repository>,
    associations: &FileAssociations,
    path: &Path,
    colors: Palette,
    size: f32,
) -> Icon {
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

    if crate::image_viewer::is_image(path) {
        return glyph(IconName::Image, 0x117065, 0x58c2b6);
    }
    if audio_preview::is_audio(path) {
        return glyph(IconName::Music, 0xa63349, 0xf17d94);
    }
    let resources = LanguageResources::for_file(repository, path);
    let association = associated_highlight(associations, resources, path).map(|(_, syntax)| syntax);
    let language = association.as_deref().unwrap_or_else(|| language_for(path));
    let extension = path.extension().and_then(|extension| extension.to_str());
    match language {
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
        "c" if extension == Some("c") || association.is_some() => {
            mark(include_bytes!("file_icons/c.svg"), 0x215ba2, 0x83b7ff)
        }
        "cpp" if matches!(extension, Some("h" | "H" | "hh" | "hpp" | "hxx" | "h++")) => {
            mark(include_bytes!("file_icons/header.svg"), 0x693b9e, 0xc8a3f6)
        }
        "cpp" if extension != Some("mm") || association.is_some() => {
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
        "text" if association.is_none() && resources.is_source_file(path) => {
            icon(IconName::FileCode, colors.muted, size)
        }
        "text" => Icon::default()
            .data(include_bytes!("file_icons/text.svg"))
            .with_size(px(size))
            .text_color(colors.muted)
            .flex_shrink_0(),
        _ => icon(IconName::FileCode, colors.muted, size),
    }
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn associated_file_uses_the_same_icon_as_its_target_language() -> anyhow::Result<()> {
    use std::sync::Arc;

    use gpui::{
        AppContext as _, Context, HeadlessAppContext, ParentElement as _, Render, Window, div, size,
    };
    use gpui_component::ThemeMode;
    use gpui_wgpu::WgpuHeadlessRenderer;

    use crate::config::ConfigFile;

    struct IconProbe {
        config: ConfigFile,
        name: &'static str,
    }

    impl Render for IconProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            let colors = Palette::for_mode(ThemeMode::Light);
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(colors.bg)
                .child(file_type_icon(
                    None,
                    &self.config.file_associations,
                    Path::new(self.name),
                    colors,
                    16.,
                ))
        }
    }

    let fixture = tempfile::tempdir()?;
    let path = fixture.path().join("config.toml");
    std::fs::write(
        &path,
        "[files.associations]\n\"*.ux\" = \"html\"\n\"photo.png\" = \"html\"\n",
    )?;
    let config = ConfigFile::load(Some(path));
    let mut app = HeadlessAppContext::with_platform(
        crate::test_support::text_system(true),
        Arc::new(gpui_kit_assets::AllAssets),
        || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
    );
    app.update(gpui_component::init);
    let mut probe = None;
    let window = app.open_window(size(px(160.), px(80.)), |_, cx| {
        let view = cx.new(|_| IconProbe {
            config,
            name: "mapped.ux",
        });
        probe = Some(view.clone());
        view
    })?;
    let probe = probe.unwrap();
    let mut paint = |name| -> anyhow::Result<_> {
        app.update(|cx| {
            probe.update(cx, |probe, cx| {
                probe.name = name;
                cx.notify();
            })
        });
        app.run_until_parked();
        app.capture_screenshot(window.into())
    };
    let html = paint("reference.html")?;
    assert!(paint("mapped.ux")? == html);
    assert!(paint("unmatched.txt")? != html);
    let image = paint("reference.png")?;
    assert!(paint("photo.png")? == image);
    assert!(image != html);
    Ok(())
}
