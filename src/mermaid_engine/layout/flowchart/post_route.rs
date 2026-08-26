use std::collections::BTreeMap;

use crate::mermaid_engine::config::LayoutConfig;
use crate::mermaid_engine::ir::{DiagramKind, Graph};

use super::super::{EdgeLayout, NodeLayout, TextBlock, resolve_edge_style};
use super::path_cleanup::{
    deoverlap_flowchart_paths, detour_flowchart_paths_around_non_endpoint_nodes,
    reduce_orthogonal_path_crossings, simplify_flowchart_axis_oscillations,
    simplify_flowchart_detour_rectangles,
};

pub(in crate::mermaid_engine::layout) fn apply_edge_path_cleanup(
    graph: &Graph,
    nodes: &BTreeMap<String, NodeLayout>,
    routed_points: &mut [Vec<(f32, f32)>],
    config: &LayoutConfig,
) {
    if graph.kind == DiagramKind::Flowchart {
        reduce_orthogonal_path_crossings(graph, nodes, routed_points, config);
        deoverlap_flowchart_paths(graph, nodes, routed_points, config);
        simplify_flowchart_detour_rectangles(graph, nodes, routed_points);
        simplify_flowchart_axis_oscillations(routed_points);
        detour_flowchart_paths_around_non_endpoint_nodes(graph, nodes, routed_points, config);
        simplify_flowchart_axis_oscillations(routed_points);
    } else if matches!(
        graph.kind,
        DiagramKind::Class | DiagramKind::Er | DiagramKind::State
    ) {
        reduce_orthogonal_path_crossings(graph, nodes, routed_points, config);
        if graph.kind == DiagramKind::Er {
            deoverlap_flowchart_paths(graph, nodes, routed_points, config);
        }
    }
}

pub(in crate::mermaid_engine::layout) fn build_edge_layouts(
    graph: &Graph,
    routed_points: &[Vec<(f32, f32)>],
    edge_route_labels: &[Option<TextBlock>],
    edge_start_labels: &[Option<TextBlock>],
    edge_end_labels: &[Option<TextBlock>],
    label_anchors: &[Option<(f32, f32)>],
    config: &LayoutConfig,
) -> Vec<EdgeLayout> {
    let mut edges = Vec::with_capacity(graph.edges.len());
    for (idx, edge) in graph.edges.iter().enumerate() {
        let label = edge_route_labels[idx].clone();
        let start_label = edge_start_labels[idx].clone();
        let end_label = edge_end_labels[idx].clone();
        let mut override_style = resolve_edge_style(idx, graph);
        if graph.kind == DiagramKind::Requirement {
            let is_contains = edge.label.as_deref() == Some("contains");
            if override_style.stroke.is_none() {
                override_style.stroke = Some(config.requirement.edge_stroke.clone());
            }
            override_style.stroke_width = Some(
                override_style
                    .stroke_width
                    .unwrap_or(config.requirement.edge_stroke_width),
            );
            if !is_contains && override_style.dasharray.is_none() {
                override_style.dasharray = Some(config.requirement.edge_dasharray.clone());
            }
            if override_style.label_color.is_none() {
                override_style.label_color = Some(config.requirement.edge_label_color.clone());
            }
        }
        edges.push(EdgeLayout {
            from: edge.from.clone(),
            to: edge.to.clone(),
            label,
            start_label,
            end_label,
            points: routed_points[idx].clone(),
            directed: edge.directed,
            arrow_start: edge.arrow_start,
            arrow_end: edge.arrow_end,
            arrow_start_kind: edge.arrow_start_kind,
            arrow_end_kind: edge.arrow_end_kind,
            start_decoration: edge.start_decoration,
            end_decoration: edge.end_decoration,
            style: edge.style,
            override_style,
            label_anchor: label_anchors[idx],
            start_label_anchor: None,
            end_label_anchor: None,
        });
    }
    edges
}

#[cfg(all(test, feature = "mermaid_engine_internal_tests"))]
mod tests {
    use super::build_edge_layouts;
    use crate::mermaid_engine::config::LayoutConfig;
    use crate::mermaid_engine::ir::{DiagramKind, EdgeStyle, Graph, NodeShape};
    use crate::mermaid_engine::layout::TextBlock;

    #[test]
    fn build_edge_layouts_applies_requirement_defaults() {
        let mut graph = Graph::new();
        graph.kind = DiagramKind::Requirement;
        graph.ensure_node("A", Some("A".to_string()), Some(NodeShape::Rectangle));
        graph.ensure_node("B", Some("B".to_string()), Some(NodeShape::Rectangle));
        graph.edges.push(crate::mermaid_engine::ir::Edge {
            from: "A".to_string(),
            to: "B".to_string(),
            label: Some("requires".to_string()),
            start_label: None,
            end_label: None,
            directed: true,
            arrow_start: false,
            arrow_end: true,
            arrow_start_kind: None,
            arrow_end_kind: None,
            start_decoration: None,
            end_decoration: None,
            style: EdgeStyle::Solid,
        });

        let config = LayoutConfig::default();
        let edges = build_edge_layouts(
            &graph,
            &[vec![(0.0, 0.0), (10.0, 0.0)]],
            &[Some(TextBlock {
                lines: vec!["requires".to_string()],
                width: 30.0,
                height: 10.0,
            })],
            &[None],
            &[None],
            &[Some((5.0, 0.0))],
            &config,
        );

        assert_eq!(
            edges[0].override_style.stroke.as_deref(),
            Some(config.requirement.edge_stroke.as_str())
        );
        assert_eq!(
            edges[0].override_style.stroke_width,
            Some(config.requirement.edge_stroke_width)
        );
        assert_eq!(
            edges[0].override_style.label_color.as_deref(),
            Some(config.requirement.edge_label_color.as_str())
        );
    }

    #[test]
    fn build_edge_layouts_keeps_requirement_contains_solid() {
        let mut graph = Graph::new();
        graph.kind = DiagramKind::Requirement;
        graph.ensure_node("A", Some("A".to_string()), Some(NodeShape::Rectangle));
        graph.ensure_node("B", Some("B".to_string()), Some(NodeShape::Rectangle));
        graph.edges.push(crate::mermaid_engine::ir::Edge {
            from: "A".to_string(),
            to: "B".to_string(),
            label: Some("contains".to_string()),
            start_label: None,
            end_label: None,
            directed: true,
            arrow_start: true,
            arrow_end: false,
            arrow_start_kind: None,
            arrow_end_kind: None,
            start_decoration: None,
            end_decoration: None,
            style: EdgeStyle::Solid,
        });

        let config = LayoutConfig::default();
        let edges = build_edge_layouts(
            &graph,
            &[vec![(0.0, 0.0), (10.0, 0.0)]],
            &[Some(TextBlock {
                lines: vec!["<<contains>>".to_string()],
                width: 70.0,
                height: 10.0,
            })],
            &[None],
            &[None],
            &[Some((5.0, 0.0))],
            &config,
        );

        assert_eq!(edges[0].override_style.dasharray, None);
    }
}
