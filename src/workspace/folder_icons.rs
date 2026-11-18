use std::{path::Path, sync::OnceLock};

use gpui::{
    AnyElement, AssetSource as _, Hsla, IntoElement as _, ParentElement as _, Styled as _, div, px,
    rgb,
};
use gpui_component::{Icon, Sizable as _};
use gpui_kit_assets::{AllAssets, IconName};

use super::{Palette, icon};

const FOLDER_SIZE: f32 = 15.;
const MARK_SIZE: f32 = 10.;
const MARK_OFFSET: f32 = 6.;

struct FolderTheme {
    mark: IconName,
    light: u32,
    dark: u32,
    outline: &'static str,
    closed: OnceLock<Icon>,
    open: OnceLock<Icon>,
}

impl FolderTheme {
    const fn new(mark: IconName, light: u32, dark: u32, outline: &'static str) -> Self {
        Self {
            mark,
            light,
            dark,
            outline,
            closed: OnceLock::new(),
            open: OnceLock::new(),
        }
    }

    fn folder(&self, name: IconName, expanded: bool) -> Icon {
        let cached = if expanded { &self.open } else { &self.closed };
        cached
            .get_or_init(|| {
                let Ok(Some(asset)) = AllAssets.load(name.path().as_ref()) else {
                    return Icon::default().path(name.path()).with_size(px(FOLDER_SIZE));
                };
                let Ok(asset) = std::str::from_utf8(&asset) else {
                    return Icon::default().path(name.path()).with_size(px(FOLDER_SIZE));
                };
                Icon::default()
                    .data(self.masked_svg(asset).as_bytes())
                    .with_size(px(FOLDER_SIZE))
            })
            .clone()
    }

    fn masked_svg(&self, folder: &str) -> String {
        // Cut out the entire overlap so every row state shows through the mark.
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">
<mask id="mark" maskUnits="userSpaceOnUse" x="0" y="0" width="24" height="24">
<rect width="24" height="24" fill="white"/>
<polygon points="{}" fill="black"/>
</mask><g mask="url(#mark)">{folder}</g></svg>"#,
            self.outline
        )
    }
}

// Filled envelopes around each mark with a one-pixel margin, projected into
// the folder's 24-unit viewBox. Each envelope includes the mark's inner gaps.
static SOURCE: FolderTheme = FolderTheme::new(
    IconName::Code,
    0x215ba2,
    0x83b7ff,
    concat!(
        "8.67,17.27 9.14,16.14 13.47,11.80 14.60,11.33 20.60,11.33 21.73,11.80 26.06,16.14 ",
        "26.53,17.27 26.53,17.93 26.06,19.06 21.73,23.40 20.60,23.87 14.60,23.87 13.47,23.40 ",
        "9.14,19.06 8.67,17.93",
    ),
);
static DOCS: FolderTheme = FolderTheme::new(
    IconName::BookOpen,
    0x215ba2,
    0x83b7ff,
    concat!(
        "8.67,11.77 8.84,11.05 9.00,10.72 9.47,10.15 10.14,9.65 11.10,9.33 24.10,9.33 ",
        "24.82,9.50 25.15,9.67 25.71,10.14 26.21,10.81 26.53,11.77 26.53,21.43 26.21,22.39 ",
        "25.71,23.06 25.15,23.53 24.82,23.70 24.55,23.80 17.72,25.80 16.77,25.79 10.61,23.79 ",
        "10.14,23.55 9.47,23.05 9.00,22.48 8.84,22.15 8.67,21.43",
    ),
);
static TESTS: FolderTheme = FolderTheme::new(
    IconName::FlaskConical,
    0x693b9e,
    0xc8a3f6,
    concat!(
        "10.00,22.93 10.05,22.56 13.05,10.22 13.47,9.47 13.80,9.14 14.93,8.67 20.27,8.67 ",
        "21.40,9.14 21.73,9.47 22.15,10.21 25.15,22.04 25.20,22.43 25.20,23.60 24.81,24.64 ",
        "23.81,25.81 23.32,26.20 22.98,26.36 22.27,26.53 12.93,26.53 11.89,26.15 10.73,25.15 ",
        "10.34,24.65 10.17,24.32 10.00,23.60",
    ),
);
static EXAMPLES: FolderTheme = FolderTheme::new(
    IconName::Play,
    0x457039,
    0xa1c891,
    concat!(
        "10.67,12.27 10.84,11.55 11.00,11.22 11.30,10.80 12.14,9.97 12.76,9.58 13.26,9.42 ",
        "13.77,9.33 14.93,9.33 15.74,9.55 24.07,14.38 24.48,14.73 25.48,15.89 25.87,16.93 ",
        "25.87,18.27 25.48,19.31 24.48,20.47 24.07,20.82 15.74,25.65 14.93,25.87 13.77,25.87 ",
        "13.26,25.78 12.76,25.62 12.14,25.23 11.30,24.40 11.00,23.98 10.84,23.65 10.67,22.93",
    ),
);
static VENDOR: FolderTheme = FolderTheme::new(
    IconName::Package,
    0x8f692d,
    0xd9b679,
    concat!(
        "9.33,14.27 9.50,13.55 9.67,13.22 10.24,12.58 15.74,9.08 16.09,8.92 16.59,8.75 ",
        "17.28,8.68 18.78,8.84 19.44,9.07 24.60,12.24 25.05,12.64 25.55,13.31 25.87,14.27 ",
        "25.87,20.93 25.48,21.97 24.48,23.14 24.06,23.49 19.39,26.16 18.78,26.36 17.28,26.52 ",
        "16.59,26.45 16.09,26.28 15.76,26.13 10.60,22.96 10.15,22.56 9.65,21.89 9.33,20.93",
    ),
);
static CMAKE: FolderTheme = FolderTheme::new(
    IconName::Wrench,
    0x9c592d,
    0xe9a16d,
    concat!(
        "8.67,22.27 8.90,21.44 15.07,11.27 15.46,10.83 17.62,9.17 18.09,8.92 18.59,8.75 ",
        "19.20,8.67 21.87,8.84 22.90,9.30 25.90,12.30 26.32,13.05 26.49,13.71 26.53,14.21 ",
        "26.36,16.71 26.05,17.56 24.55,19.56 24.10,19.96 13.77,26.30 12.93,26.53 11.77,26.53 ",
        "10.84,26.24 9.67,25.40 9.17,24.82 8.84,24.15 8.67,23.43",
    ),
);
static TEMP: FolderTheme = FolderTheme::new(
    IconName::Hourglass,
    0x8f692d,
    0xd9b679,
    concat!(
        "10.67,10.60 11.14,9.47 11.47,9.14 12.60,8.67 22.60,8.67 23.73,9.14 24.06,9.47 ",
        "24.53,10.60 24.53,24.60 24.06,25.73 23.73,26.06 22.60,26.53 12.60,26.53 11.47,26.06 ",
        "11.14,25.73 10.67,24.60",
    ),
);
static CACHE: FolderTheme = FolderTheme::new(
    IconName::Database,
    0x267371,
    0x78c5bb,
    concat!(
        "9.33,12.43 9.50,11.72 9.67,11.38 10.18,10.79 11.85,9.62 12.42,9.37 15.42,8.70 ",
        "15.89,8.67 22.55,9.17 23.28,9.41 24.61,10.24 25.14,10.78 25.64,11.61 25.87,12.43 ",
        "25.87,22.77 25.57,23.70 24.74,24.86 24.15,25.36 23.48,25.70 22.91,25.86 15.58,26.53 ",
        "15.09,26.50 12.09,25.83 11.50,25.57 10.34,24.74 9.84,24.15 9.50,23.48 9.33,22.77",
    ),
);
static BUILD: FolderTheme = FolderTheme::new(
    IconName::Hammer,
    0x9c592d,
    0xe9a16d,
    concat!(
        "8.50,22.43 8.63,21.80 13.46,10.63 13.80,10.14 14.14,9.80 15.38,9.34 20.05,9.67 ",
        "20.76,9.89 21.59,10.39 21.98,10.72 26.15,15.56 26.53,16.60 26.53,17.27 26.06,18.40 ",
        "25.23,19.23 24.94,19.46 13.77,26.30 12.93,26.53 11.77,26.53 10.84,26.24 9.67,25.40 ",
        "9.23,24.92 8.73,24.09 8.50,23.27",
    ),
);

fn folder_theme(path: &Path) -> Option<&'static FolderTheme> {
    if path.parent() != Some(Path::new("")) {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    Some(match name {
        "src" => &SOURCE,
        "docs" => &DOCS,
        "tests" => &TESTS,
        "examples" => &EXAMPLES,
        "vendor" => &VENDOR,
        "cmake" => &CMAKE,
        "temp" | "tmp" => &TEMP,
        ".cache" | "cache" => &CACHE,
        ".build" | "build" | "target" => &BUILD,
        name if name.starts_with("cmake-build-") => &BUILD,
        _ => return None,
    })
}

pub(super) fn folder_icon(path: &Path, expanded: bool, color: Hsla, colors: Palette) -> AnyElement {
    let name = if expanded {
        IconName::FolderOpen
    } else {
        IconName::Folder
    };
    let Some(theme) = folder_theme(path) else {
        return icon(name, color, FOLDER_SIZE).into_any_element();
    };
    let tint = if colors.bg.l < 0.5 {
        theme.dark
    } else {
        theme.light
    };
    div()
        .relative()
        .size(px(FOLDER_SIZE))
        .flex_shrink_0()
        .child(theme.folder(name, expanded).text_color(color))
        .child(
            div()
                .absolute()
                .left(px(MARK_OFFSET))
                .top(px(MARK_OFFSET))
                .child(icon(theme.mark, rgb(tint).into(), MARK_SIZE)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::SvgRenderer;

    use super::*;

    #[test]
    fn hourglass_envelope_clears_folder_strokes_inside_the_mark() -> anyhow::Result<()> {
        let renderer = SvgRenderer::new(Arc::new(AllAssets));
        for name in [IconName::Folder, IconName::FolderOpen] {
            let svg = AllAssets.load(name.path().as_ref())?.unwrap();
            let base = renderer.render_single_frame(&svg, 1.)?;
            let masked_svg = TEMP.masked_svg(std::str::from_utf8(&svg)?);
            let masked = renderer.render_single_frame(masked_svg.as_bytes(), 1.)?;
            let width = base.size(0).width.0 as usize;
            assert_eq!(masked.size(0), base.size(0));
            let base = base.as_bytes(0).unwrap();
            let masked = masked.as_bytes(0).unwrap();
            let mut removed_stroke = false;
            for (index, (before, after)) in
                base.chunks_exact(4).zip(masked.chunks_exact(4)).enumerate()
            {
                let x = 24. * (index % width) as f32 / width as f32;
                let y = 24. * (index / width) as f32 / width as f32;
                if (12. ..23.).contains(&x) && (11. ..22.).contains(&y) {
                    assert_eq!(after[3], 0, "{name:?} at ({x}, {y})");
                    removed_stroke |= before[3] != 0;
                } else if x < 8. {
                    assert_eq!(before[3], after[3], "{name:?} at ({x}, {y})");
                }
            }
            assert!(
                removed_stroke,
                "{name:?} must overlap the hourglass envelope"
            );
        }
        Ok(())
    }
}
