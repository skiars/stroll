//! Flowchart edge path renderer.

use super::super::defs::write_flowchart_marker_id_xml;
use super::super::*;
#[cfg(test)]
use merman_core::diagrams::flowchart::{FlowEdgeMarker, FlowEdgeStroke, FlowEdgeVisibility};

pub(in crate::svg::parity::flowchart) fn render_flowchart_edge_path(
    out: &mut String,
    ctx: &FlowchartRenderCtx<'_>,
    edge: &crate::flowchart::FlowEdge,
    origin_x: f64,
    origin_y: f64,
    scratch: &mut FlowchartEdgeDataPointsScratch,
    edge_cache: &mut FxHashMap<&str, FlowchartEdgePathCacheEntry>,
) -> crate::Result<()> {
    let trace_enabled = ctx.trace_edge_id.is_some_and(|id| id == edge.id.as_str());

    let cached_geom = edge_cache
        .get(edge.id.as_str())
        .filter(|c| (c.origin_x - origin_x).abs() <= 1e-9 && (c.origin_y - origin_y).abs() <= 1e-9)
        .map(|c| &c.geom);

    // Trace collection recomputes the geometry before graph-wide postprocessing for diagnostics, but the emitted SVG
    // must still consume the post-processed cache. Enabling diagnostics must not alter rendering.
    let owned_geom = if cached_geom.is_none() || trace_enabled {
        if let Some(layout_edge) = ctx.layout_edges_by_id.get(edge.id.as_str()) {
            ctx.work_meter.charge(layout_edge.points.len().max(2))?;
        }
        flowchart_compute_edge_path_geom(
            FlowchartEdgePathGeomRequest {
                ctx,
                edge,
                origin_x,
                origin_y,
                trace_enabled,
            },
            scratch,
        )
    } else {
        None
    };
    let geom = if let Some(g) = cached_geom {
        g
    } else {
        let Some(g) = owned_geom.as_ref() else {
            return Ok(());
        };
        g
    };
    let d = geom.d.as_str();
    let data_points_b64 = geom.data_points_b64.as_str();
    let data_look = flowchart_config_look(ctx.config);
    let hand_drawn = data_look == "handDrawn";
    let rough_d = if hand_drawn && !geom.line_hop_applied {
        super::node::roughjs::roughjs_hand_drawn_stroke_path_for_svg_path(
            d,
            0.3,
            &ctx.hand_drawn_seed,
        )
    } else {
        None
    };
    let d = rough_d.as_deref().unwrap_or(d);

    let mut marker_color: Option<&str> = None;
    for raw in ctx.default_edge_style.iter().chain(edge.style.iter()) {
        // Mirror Mermaid: handDrawn passes the full `stroke:...` style token to edgeMarker,
        // while classic/neo passes the captured color value from final pathStyle.
        let s = raw.trim_start();
        let Some(rest) = s.strip_prefix("stroke:") else {
            continue;
        };
        if !rest.trim().is_empty() {
            marker_color = Some(if hand_drawn { s } else { rest });
            break;
        }
    }

    // If no inline `stroke:` exists, Mermaid still colors markers based on class-derived stroke
    // styles (see `edges.js` `stylesFromClasses` + `edgeMarker.ts` `strokeColor` extraction).
    // We approximate this by compiling the edge styles using class defs and reusing the resulting
    // `stroke` value for the marker id suffix.
    let compiled_marker_color = if !hand_drawn && marker_color.is_none() && !edge.classes.is_empty()
    {
        flowchart_resolve_stroke_for_marker(
            ctx.class_defs,
            &edge.classes,
            &ctx.default_edge_style,
            &edge.style,
        )
    } else {
        None
    };
    if marker_color.is_none() {
        marker_color = compiled_marker_color.as_deref();
    }

    fn write_style_joined(out: &mut String, a: &[String], b: &[String]) {
        let mut first = true;
        for part in a.iter().chain(b.iter()) {
            if first {
                first = false;
            } else {
                out.push(';');
            }
            // Decode each original token before adding CSS delimiters: a trailing semicolon
            // would make a literal hex color look like a Mermaid entity (#abcdef;).
            out.push_str(crate::entities::decode_mermaid_entities_for_render_text(part).as_ref());
        }
    }

    scratch.edge_style.clear();
    if data_look == "neo"
        && !flowchart_edge_is_animated(ctx, edge)
        && let Some(path_length) = geom.original_path_length
    {
        write_flowchart_neo_edge_mask(&mut scratch.edge_style, path_length, edge);
    }
    if hand_drawn {
        write_style_joined(
            &mut scratch.edge_style,
            &ctx.default_edge_style,
            &edge.style,
        );
    } else if ctx.default_edge_style.is_empty() && edge.style.is_empty() {
        scratch.edge_style.push(';');
    } else {
        let style_start = scratch.edge_style.len();
        write_style_joined(
            &mut scratch.edge_style,
            &ctx.default_edge_style,
            &edge.style,
        );
        let style_end = scratch.edge_style.len();
        scratch.edge_style.push_str(";;;");
        scratch
            .edge_style
            .extend_from_within(style_start..style_end);
    }
    // lineJump.ts rewrites the complete painted style after replacing the path. In
    // particular, its first four dash numbers can come from a repeated dotted pattern.
    let style = if geom.line_hop_applied {
        crate::svg::parity::line_hops::rewrite_style_after_line_hop(
            &scratch.edge_style,
            d,
            ctx.work_meter,
        )?
    } else {
        std::borrow::Cow::Borrowed(scratch.edge_style.as_str())
    };
    let gap = if let Some(bounds) =
        super::edge_label::reader_edge_label_gap_bounds(ctx, edge, geom, d, origin_x, origin_y)
    {
        crate::svg::parity::edge_gap::cut_label_gap(d, bounds, ctx.work_meter)?
    } else {
        None
    };

    let marker_start = flowchart_edge_marker_start_base(edge);
    let marker_end = flowchart_edge_marker_end_base(edge);
    let animated = flowchart_edge_is_animated(ctx, edge);
    let write_path = |out: &mut String,
                      d: &str,
                      style: &str,
                      primary: bool,
                      start: Option<&str>,
                      end: Option<&str>| {
        let _ = write!(out, r#"<path d="{d}""#);
        if primary {
            let _ = write!(
                out,
                r#" id="{}-{}""#,
                ctx.diagram_id,
                escape_xml_display(&edge.id)
            );
        }
        out.push_str(r#" class=""#);
        css::write_flowchart_edge_class_attr(out, edge);
        if hand_drawn {
            out.push_str(" transition");
        }
        let _ = write!(out, r#"" style="{}"#, escape_attr_display(style));
        if hand_drawn {
            out.push_str(r##"" stroke="#000" stroke-width="1" fill="none"##);
        }
        out.push('"');
        if primary {
            out.push_str(r#" data-edge="true" data-et="edge""#);
        }
        let _ = write!(out, r#" data-id="{}""#, escape_xml_display(&edge.id));
        if primary {
            let _ = write!(out, r#" data-points="{data_points_b64}""#);
        }
        let _ = write!(out, r#" data-look="{}""#, escape_xml_display(data_look));
        for (side, base) in [("start", start), ("end", end)] {
            if let Some(base) = base {
                let _ = write!(out, r#" marker-{side}="url(#"#);
                write_flowchart_marker_id_xml(
                    out,
                    ctx.diagram_id,
                    ctx.diagram_type,
                    base,
                    data_look == "neo" && !animated,
                    marker_color,
                );
                out.push_str(r#")""#);
            }
        }
        out.push_str(" />");
    };

    if let Some(gap) = gap {
        let pieces = gap.visible.len();
        let mut primary = true;
        for (index, part) in gap.visible.iter().enumerate() {
            let mut painted_style = std::borrow::Cow::Borrowed(style.as_ref());
            if !animated && part.dash_offset > 0.0 {
                painted_style = dash_style_at_distance(&style, part.dash_offset);
            }
            write_path(
                out,
                &part.path.to_svg(),
                &painted_style,
                primary,
                marker_start.filter(|_| index == 0 && gap.start_marker_path.is_none()),
                marker_end.filter(|_| index + 1 == pieces && gap.end_marker_path.is_none()),
            );
            primary = false;
        }
        for (path, start, end) in [
            (gap.start_marker_path, marker_start, None),
            (gap.end_marker_path, None, marker_end),
        ] {
            if let Some(path) = path.filter(|_| start.is_some() || end.is_some()) {
                let marker_style = format!("{style};fill:none!important;stroke:none!important;");
                write_path(out, &path.to_svg(), &marker_style, primary, start, end);
                primary = false;
            }
        }
    } else {
        write_path(out, d, &style, true, marker_start, marker_end);
    }
    ctx.checkpoint_emit()?;

    if let Some(emitted_d_for_label) = rough_d
        && let Some(cache_entry) = edge_cache.get_mut(edge.id.as_str())
        && (cache_entry.origin_x - origin_x).abs() <= 1e-9
        && (cache_entry.origin_y - origin_y).abs() <= 1e-9
    {
        cache_entry.geom.emitted_d_for_label = Some(emitted_d_for_label);
    }
    Ok(())
}

fn dash_style_at_distance(style: &str, distance: f64) -> std::borrow::Cow<'_, str> {
    fn dash_offset(declaration: &str) -> Option<&str> {
        let (name, value) = declaration.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("stroke-dashoffset")
            .then(|| value.trim())
    }

    let offset = style.rsplit(';').find_map(dash_offset);
    let (offset, priority) = offset
        .and_then(|value| value.strip_suffix("!important"))
        .map_or((offset.unwrap_or("0"), ""), |value| {
            (value.trim(), " !important")
        });
    let Ok(offset) = offset.strip_suffix("px").unwrap_or(offset).parse::<f64>() else {
        return std::borrow::Cow::Borrowed(style);
    };

    let mut adjusted = String::with_capacity(style.len() + 32);
    for declaration in style
        .split(';')
        .filter(|value| dash_offset(value).is_none())
    {
        adjusted.push_str(declaration);
        adjusted.push(';');
    }
    let _ = write!(
        adjusted,
        "stroke-dashoffset:{}{priority};",
        fmt_display(offset + distance)
    );
    std::borrow::Cow::Owned(adjusted)
}

fn write_flowchart_neo_edge_mask(
    out: &mut String,
    path_length: f64,
    edge: &crate::flowchart::FlowEdge,
) {
    let (arrow_type_start, arrow_type_end) =
        super::super::edge_geom::arrow_types_for_edge(edge.edge_type.as_deref());
    crate::svg::parity::edge_path::write_neo_edge_mask(
        out,
        path_length,
        arrow_type_start,
        arrow_type_end,
        matches!(edge.stroke.as_deref(), Some("dotted" | "dashed")),
        false,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(edge_type: &str, stroke: &str) -> crate::flowchart::FlowEdge {
        crate::flowchart::FlowEdge {
            id: "edge".to_string(),
            from: "A".to_string(),
            to: "B".to_string(),
            label: None,
            label_type: None,
            edge_type: Some(edge_type.to_string()),
            arrow: String::new(),
            start_marker: FlowEdgeMarker::None,
            end_marker: if edge_type.starts_with("arrow_open") {
                FlowEdgeMarker::None
            } else {
                FlowEdgeMarker::Point
            },
            is_user_defined_id: false,
            stroke: Some(stroke.to_string()),
            stroke_kind: match stroke {
                "dotted" => FlowEdgeStroke::Dotted,
                "thick" => FlowEdgeStroke::Thick,
                _ => FlowEdgeStroke::Normal,
            },
            visibility: if stroke == "invisible" {
                FlowEdgeVisibility::Invisible
            } else {
                FlowEdgeVisibility::Visible
            },
            interpolate: None,
            classes: Vec::new(),
            style: Vec::new(),
            animate: None,
            animation: None,
            length: 1,
        }
    }

    #[test]
    fn neo_solid_mask_uses_original_length_and_marker_offsets() {
        let mut style = String::new();
        write_flowchart_neo_edge_mask(&mut style, 40.0, &edge("arrow_point", "normal"));
        assert_eq!(style, "stroke-dasharray: 0 0 36 4; stroke-dashoffset: 0;");

        style.clear();
        write_flowchart_neo_edge_mask(&mut style, 50.0, &edge("double_arrow_circle", "normal"));
        assert_eq!(
            style,
            "stroke-dasharray: 0 12.5 25 12.5; stroke-dashoffset: 0;"
        );
    }

    #[test]
    fn neo_dotted_mask_preserves_upstream_two_pixel_pattern() {
        let mut style = String::new();
        write_flowchart_neo_edge_mask(&mut style, 16.0, &edge("arrow_open", "dotted"));
        assert_eq!(
            style,
            "stroke-dasharray: 0 0 2 2 2 2 2 2 2 2 0; stroke-dashoffset: 0;"
        );
    }
}
