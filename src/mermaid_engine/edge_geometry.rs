use crate::mermaid_engine::ir::{DiagramKind, EdgeArrowhead};

/// How many pixels the arrowhead marker penetrates past the path endpoint.
///
/// This belongs in shared edge geometry instead of the SVG renderer because the
/// layout router must shorten routed paths before markers are painted, while the
/// renderer owns only the final marker definitions.
pub(crate) fn arrowhead_inset(kind: DiagramKind, arrow_kind: Option<EdgeArrowhead>) -> f32 {
    match kind {
        DiagramKind::Class => match arrow_kind {
            Some(EdgeArrowhead::OpenTriangle) => 17.0,
            Some(EdgeArrowhead::ClassDependency) => 5.0,
            None => 4.0,
        },
        _ => 0.0,
    }
}

/// Apply start/end marker insets to an already routed path.
///
/// The operation is deliberately conservative: endpoints are only moved when the
/// adjacent segment is longer than the requested inset. Very short segments are
/// left intact to avoid reversing or collapsing route geometry.
pub(crate) fn apply_endpoint_insets(
    mut path: Vec<(f32, f32)>,
    start_inset: f32,
    end_inset: f32,
) -> Vec<(f32, f32)> {
    if start_inset > 0.0 && path.len() >= 2 {
        let (sx, sy) = path[0];
        let (nx, ny) = path[1];
        let dx = sx - nx;
        let dy = sy - ny;
        let len = (dx * dx + dy * dy).sqrt();
        if len > start_inset {
            let r = start_inset / len;
            path[0] = (sx - dx * r, sy - dy * r);
        }
    }

    if end_inset > 0.0 && path.len() >= 2 {
        let n = path.len();
        let (px, py) = path[n - 2];
        let (ex, ey) = path[n - 1];
        let dx = ex - px;
        let dy = ey - py;
        let len = (dx * dx + dy * dy).sqrt();
        if len > end_inset {
            let r = end_inset / len;
            path[n - 1] = (ex - dx * r, ey - dy * r);
        }
    }

    path
}

#[cfg(all(test, feature = "mermaid_engine_internal_tests"))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_insets_shorten_long_segments_without_collapsing_short_segments() {
        let path = vec![(0.0, 0.0), (10.0, 0.0), (20.0, 0.0)];
        let inset = apply_endpoint_insets(path, 3.0, 4.0);
        assert_eq!(inset[0], (3.0, 0.0));
        assert_eq!(inset[2], (16.0, 0.0));

        let short = vec![(0.0, 0.0), (2.0, 0.0)];
        assert_eq!(apply_endpoint_insets(short.clone(), 3.0, 3.0), short);
    }
}
