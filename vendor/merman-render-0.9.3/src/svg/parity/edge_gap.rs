use kurbo::{BezPath, Line, ParamCurve as _, ParamCurveArclen as _, PathEl, PathSeg, Rect};

use crate::{Result, resources::OperationWorkMeter};

pub(super) struct VisiblePath {
    pub(super) path: BezPath,
    pub(super) dash_offset: f64,
}

pub(super) struct LabelGap {
    pub(super) visible: Vec<VisiblePath>,
    pub(super) start_marker_path: Option<BezPath>,
    pub(super) end_marker_path: Option<BezPath>,
}

pub(super) fn cut_label_gap(
    d: &str,
    bounds: Rect,
    work: &OperationWorkMeter,
) -> Result<Option<LabelGap>> {
    let Ok(path) = BezPath::from_svg(d) else {
        return Ok(None);
    };
    work.charge(path.elements().len())?;
    Ok(remove_label_segments(&path, bounds))
}

fn marker_path(segment: PathSeg) -> BezPath {
    let mut path = BezPath::new();
    path.move_to(segment.start());
    path.push(segment.as_path_el());
    path
}

fn remove_label_segments(path: &BezPath, bounds: Rect) -> Option<LabelGap> {
    const EPSILON: f64 = 1e-8;
    const LENGTH_ACCURACY: f64 = 1e-6;
    if !path.is_finite() || !bounds.is_finite() {
        return None;
    }
    let corners = [
        (bounds.x0, bounds.y0),
        (bounds.x1, bounds.y0),
        (bounds.x1, bounds.y1),
        (bounds.x0, bounds.y1),
    ];
    let sides = std::array::from_fn::<_, 4, _>(|i| Line::new(corners[i], corners[(i + 1) % 4]));
    let mut visible = Vec::<VisiblePath>::new();
    let mut drawing = false;
    let mut removed = false;
    let mut distance = 0.0;
    let mut times: Vec<f64> = Vec::with_capacity(14);
    let mut start_marker_path = None;
    let mut end_marker_path = None;
    let mut is_first_segment = true;
    for (index, element) in path.iter().enumerate() {
        if matches!(element, PathEl::MoveTo(_)) {
            distance = 0.0;
            drawing = false;
            continue;
        }
        let Some(segment) = path.get_seg(index) else {
            continue;
        };
        times.clear();
        times.extend([0.0, 1.0]);
        for side in sides {
            times.extend(
                segment
                    .intersect_line(side)
                    .into_iter()
                    .map(|hit| hit.segment_t.clamp(0.0, 1.0)),
            );
        }
        times.sort_unstable_by(f64::total_cmp);
        times.dedup_by(|a, b| (*a - *b).abs() < EPSILON);
        for span in times.windows(2) {
            let (start, stop) = (span[0], span[1]);
            if bounds.contains(segment.eval((start + stop) / 2.0)) {
                if is_first_segment && start == 0.0 {
                    start_marker_path = Some(marker_path(segment));
                }
                drawing = false;
                removed = true;
                continue;
            }
            let piece = segment.subsegment(start..stop);
            if !drawing {
                let mut path = BezPath::new();
                path.move_to(piece.start());
                let dash_offset = distance + segment.subsegment(0.0..start).arclen(LENGTH_ACCURACY);
                visible.push(VisiblePath { path, dash_offset });
                drawing = true;
            }
            visible.last_mut().unwrap().path.push(piece.as_path_el());
        }
        // SVG markers stay at the original termini, even when the label covers a short route.
        end_marker_path = (!drawing).then(|| marker_path(segment));
        is_first_segment = false;
        distance += segment.arclen(LENGTH_ACCURACY);
    }
    removed.then_some(LabelGap {
        visible,
        start_marker_path,
        end_marker_path,
    })
}
