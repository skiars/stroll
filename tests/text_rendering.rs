#![cfg(any(target_os = "linux", target_os = "windows"))]

use gpui::{RenderGlyphParams, font, point, px};

#[test]
fn compact_ui_text_defaults_to_twelve_pixels_and_allows_explicit_overrides() {
    use gpui::{StyleRefinement, Styled as _};

    let small = StyleRefinement::default().text_sm();
    let extra_small = StyleRefinement::default().text_xs();
    let badge = StyleRefinement::default().text_xs().text_size(px(11.));

    for (style, expected) in [(small, 12.), (extra_small, 12.), (badge, 11.)] {
        assert_eq!(
            style.text.font_size.unwrap().to_pixels(px(13.5)),
            px(expected),
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn swash_fractional_baselines_move_ink_down_through_pixel_boundaries() -> anyhow::Result<()> {
    let text_system = gpui_platform::current_platform(true).text_system();
    let font_id = ["DejaVu Sans", "Liberation Sans", "Noto Sans"]
        .into_iter()
        .find_map(|family| text_system.font_id(&font(family)).ok())
        .ok_or_else(|| anyhow::anyhow!("No scalable sans-serif test font found"))?;
    let glyph_id = text_system.glyph_for_char(font_id, 'S').unwrap();

    for scale_factor in [1., 1.25] {
        for subpixel_rendering in [false, true] {
            let mut params = RenderGlyphParams {
                font_id,
                glyph_id,
                font_size: px(13.),
                subpixel_variant: point(0, 0),
                subpixel_steps: point(4, 4),
                scale_factor,
                is_emoji: false,
                subpixel_rendering,
                hinting: false,
                dilation: 0,
            };
            let mut previous = None;
            for step in 0..=4 {
                params.subpixel_variant.y = step % 4;
                let bounds = text_system.glyph_raster_bounds(&params)?;
                let (size, bitmap) = text_system.rasterize_glyph(&params, bounds)?;
                let channels = if subpixel_rendering { 4 } else { 1 };
                let top = bounds.origin.y.0 + i32::from(step / 4);
                let mut ink = 0.;
                let mut moment = 0.;
                for (row, pixels) in bitmap
                    .chunks_exact(size.width.0 as usize * channels)
                    .enumerate()
                {
                    let coverage =
                        f64::from(pixels.iter().map(|&value| u32::from(value)).sum::<u32>());
                    ink += coverage;
                    moment += coverage * (f64::from(top) + row as f64);
                }
                assert!(ink > 0.);
                let center = moment / ink;
                if let Some(previous) = previous {
                    let delta: f64 = center - previous;
                    assert!(
                        (delta - 0.25).abs() < 0.1,
                        "scale {scale_factor}, LCD {subpixel_rendering}, step {step} moved ink by {delta:.3} pixels",
                    );
                }
                previous = Some(center);
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
#[test]
fn directwrite_rasterizes_hinting_and_fractional_positions() -> anyhow::Result<()> {
    let platform = gpui_platform::current_platform(false);
    let text_system = platform.text_system();
    let font_id = text_system.font_id(&font("Segoe UI"))?;
    let glyph_id = text_system.glyph_for_char(font_id, 'S').unwrap();

    for subpixel_rendering in [false, true] {
        let mut params = RenderGlyphParams {
            font_id,
            glyph_id,
            font_size: px(13.),
            subpixel_variant: point(0, 0),
            subpixel_steps: point(4, 1),
            scale_factor: 1.,
            is_emoji: false,
            subpixel_rendering,
            hinting: true,
            dilation: 0,
        };
        let rasterize = |params: &RenderGlyphParams| -> anyhow::Result<_> {
            let bounds = text_system.glyph_raster_bounds(params)?;
            let (_, bitmap) = text_system.rasterize_glyph(params, bounds)?;
            assert!(bitmap.iter().any(|&pixel| pixel != 0));
            Ok((bounds, bitmap))
        };

        let hinted = rasterize(&params)?;
        params.hinting = false;
        let unhinted = rasterize(&params)?;
        assert_ne!(hinted, unhinted);

        params.subpixel_steps.y = 4;
        params.subpixel_variant.y = 1;
        let quarter_pixel = rasterize(&params)?;
        assert_ne!(quarter_pixel, unhinted);

        params.subpixel_steps.y = 8;
        params.subpixel_variant.y = 2;
        assert_eq!(rasterize(&params)?, quarter_pixel);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires Google Sans variable fonts installed locally"]
fn linux_google_sans_shapes_and_rasterizes_distinct_weights() -> anyhow::Result<()> {
    use gpui::{FontRun, FontWeight};

    let text_system = gpui_platform::current_platform(true).text_system();
    let mut descriptor = font("Google Sans");
    let mut ids = Vec::new();
    let mut widths = Vec::new();
    let mut coverage = Vec::new();
    let word = "Workspace";
    let sample = word.repeat(3);
    let font_size = px(24.);

    for weight in [FontWeight::NORMAL, FontWeight::SEMIBOLD, FontWeight::BOLD] {
        descriptor.weight = weight;
        let font_id = text_system.font_id(&descriptor)?;
        assert!(!ids.contains(&font_id));
        ids.push(font_id);
        let line = text_system.layout_line(
            word,
            font_size,
            &[FontRun {
                len: word.len(),
                font_id,
            }],
        );
        widths.push(line.width);
        assert_eq!(line.runs.len(), 1);
        assert_eq!(line.runs[0].font_id, font_id);

        let glyph_id = text_system.glyph_for_char(font_id, 'W').unwrap();
        let params = RenderGlyphParams {
            font_id,
            glyph_id,
            font_size,
            subpixel_variant: point(0, 0),
            subpixel_steps: point(4, 4),
            scale_factor: 1.,
            is_emoji: false,
            subpixel_rendering: false,
            hinting: false,
            dilation: 0,
        };
        let bounds = text_system.glyph_raster_bounds(&params)?;
        let (_, bitmap) = text_system.rasterize_glyph(&params, bounds)?;
        coverage.push(bitmap.iter().map(|&pixel| u64::from(pixel)).sum::<u64>());
    }

    // Exercise neighbouring regular/semibold/bold runs as in rich text.
    let runs = ids
        .iter()
        .map(|&font_id| FontRun {
            len: word.len(),
            font_id,
        })
        .collect::<Vec<_>>();
    let line = text_system.layout_line(&sample, font_size, &runs);
    assert_eq!(
        line.runs.iter().map(|run| run.font_id).collect::<Vec<_>>(),
        ids
    );
    assert!(widths.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(coverage.windows(2).all(|pair| pair[0] < pair[1]));
    Ok(())
}
