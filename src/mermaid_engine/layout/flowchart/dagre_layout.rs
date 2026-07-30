use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use dagre::graph::{Graph as DagreGraph, GraphOptions};
use dagre::{
    EdgeLabel as DagreEdgeLabel, LayoutOptions, NodeLabel as DagreNodeLabel, RankDir, Ranker,
};

use crate::mermaid_engine::config::LayoutConfig;
use crate::mermaid_engine::ir::{DiagramKind, Direction, Graph, NodeShape};
use crate::mermaid_engine::theme::Theme;

use super::super::{EdgeLayout, Layout, NodeLayout, TextBlock, resolve_edge_style};

pub(in crate::mermaid_engine::layout) fn try_compute_flowchart_layout(
    graph: &Graph,
    mut nodes: BTreeMap<String, NodeLayout>,
    edge_route_labels: &[Option<TextBlock>],
    edge_start_labels: &[Option<TextBlock>],
    edge_end_labels: &[Option<TextBlock>],
    theme: &Theme,
    config: &LayoutConfig,
) -> Option<Layout> {
    if graph.kind != DiagramKind::Flowchart
        || !graph.subgraphs.is_empty()
        || nodes.is_empty()
        || has_multiple_weak_components(graph)
        || graph.edges.iter().any(|edge| edge.from == edge.to)
    {
        return None;
    }

    let mut dagre_graph: DagreGraph<DagreNodeLabel, DagreEdgeLabel> =
        DagreGraph::with_options(GraphOptions {
            directed: true,
            multigraph: true,
            compound: false,
        });

    for node_id in ordered_node_ids(graph) {
        let node = nodes.get(&node_id)?;
        dagre_graph.set_node(
            node_id,
            Some(DagreNodeLabel {
                width: node.width.max(1.0) as f64,
                height: node.height.max(1.0) as f64,
                ..DagreNodeLabel::default()
            }),
        );
    }

    for (idx, edge) in graph.edges.iter().enumerate() {
        if !nodes.contains_key(&edge.from) || !nodes.contains_key(&edge.to) {
            return None;
        }

        let mut label = DagreEdgeLabel::default();
        if let Some(text) = edge_route_labels.get(idx).and_then(|label| label.as_ref()) {
            label.width = text.width.max(0.0) as f64;
            label.height = text.height.max(0.0) as f64;
        }

        let edge_name = dagre_edge_name(idx);
        dagre_graph.set_edge(
            edge.from.clone(),
            edge.to.clone(),
            Some(label),
            Some(edge_name.as_str()),
        );
    }

    dagre::layout(
        &mut dagre_graph,
        Some(layout_options(graph.direction, config)),
    );

    for node_id in ordered_node_ids(graph) {
        let dagre_node = dagre_graph.node(&node_id)?;
        let x = finite_f64_to_f32(dagre_node.x?)?;
        let y = finite_f64_to_f32(dagre_node.y?)?;
        let node = nodes.get_mut(&node_id)?;
        node.x = x - node.width / 2.0;
        node.y = y - node.height / 2.0;
    }

    let mut edges = Vec::with_capacity(graph.edges.len());
    for (idx, edge) in graph.edges.iter().enumerate() {
        let edge_name = dagre_edge_name(idx);
        let dagre_edge = dagre_graph.edge(&edge.from, &edge.to, Some(edge_name.as_str()))?;
        let mut points: Vec<(f32, f32)> = dagre_edge
            .points
            .iter()
            .filter_map(|point| Some((finite_f64_to_f32(point.x)?, finite_f64_to_f32(point.y)?)))
            .collect();
        if points.len() < 2 {
            points = fallback_edge_points(&nodes, &edge.from, &edge.to)?;
        }
        adjust_edge_endpoints_to_node_shapes(
            &mut points,
            nodes.get(&edge.from)?,
            nodes.get(&edge.to)?,
        );

        edges.push(EdgeLayout {
            from: edge.from.clone(),
            to: edge.to.clone(),
            label: edge_route_labels.get(idx).cloned().unwrap_or(None),
            start_label: edge_start_labels.get(idx).cloned().unwrap_or(None),
            end_label: edge_end_labels.get(idx).cloned().unwrap_or(None),
            label_anchor: match (dagre_edge.x, dagre_edge.y) {
                (Some(x), Some(y)) => Some((finite_f64_to_f32(x)?, finite_f64_to_f32(y)?)),
                _ => None,
            },
            start_label_anchor: None,
            end_label_anchor: None,
            points,
            directed: edge.directed,
            arrow_start: edge.arrow_start,
            arrow_end: edge.arrow_end,
            arrow_start_kind: edge.arrow_start_kind,
            arrow_end_kind: edge.arrow_end_kind,
            start_decoration: edge.start_decoration,
            end_decoration: edge.end_decoration,
            style: edge.style,
            override_style: resolve_edge_style(idx, graph),
        });
    }

    let mut finalize_graph = graph.clone();
    finalize_graph.direction = Direction::TopDown;
    Some(super::finalize::finalize_graph_layout(
        &finalize_graph,
        nodes,
        edges,
        Vec::new(),
        theme,
        config,
    ))
}

fn ordered_node_ids(graph: &Graph) -> Vec<String> {
    let mut ids: Vec<String> = graph.nodes.keys().cloned().collect();
    ids.sort_by(|a, b| {
        graph
            .node_order
            .get(a)
            .copied()
            .unwrap_or(usize::MAX)
            .cmp(&graph.node_order.get(b).copied().unwrap_or(usize::MAX))
            .then_with(|| a.cmp(b))
    });
    ids
}

fn has_multiple_weak_components(graph: &Graph) -> bool {
    let mut adjacency: HashMap<&str, Vec<&str>> = graph
        .nodes
        .keys()
        .map(|id| (id.as_str(), Vec::new()))
        .collect();
    for edge in &graph.edges {
        if !graph.nodes.contains_key(&edge.from) || !graph.nodes.contains_key(&edge.to) {
            continue;
        }
        adjacency
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
        adjacency
            .entry(edge.to.as_str())
            .or_default()
            .push(edge.from.as_str());
    }

    let mut seen: HashSet<&str> = HashSet::new();
    let mut components = 0usize;
    for node_id in graph.nodes.keys().map(String::as_str) {
        if !seen.insert(node_id) {
            continue;
        }
        components += 1;
        if components > 1 {
            return true;
        }
        let mut queue = VecDeque::from([node_id]);
        while let Some(current) = queue.pop_front() {
            if let Some(neighbors) = adjacency.get(current) {
                for &next in neighbors {
                    if seen.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }
    }

    false
}

fn layout_options(direction: Direction, config: &LayoutConfig) -> LayoutOptions {
    LayoutOptions {
        rankdir: match direction {
            Direction::TopDown => RankDir::TB,
            Direction::LeftRight => RankDir::LR,
            Direction::BottomTop => RankDir::BT,
            Direction::RightLeft => RankDir::RL,
        },
        nodesep: config.node_spacing.max(1.0) as f64,
        ranksep: config.rank_spacing.max(1.0) as f64,
        edgesep: (config.node_spacing * 0.4).max(10.0) as f64,
        marginx: 0.0,
        marginy: 0.0,
        ranker: Ranker::NetworkSimplex,
        tie_keep_first: true,
        ..LayoutOptions::default()
    }
}

fn dagre_edge_name(idx: usize) -> String {
    format!("e{idx}")
}

fn finite_f64_to_f32(value: f64) -> Option<f32> {
    value.is_finite().then_some(value as f32)
}

fn fallback_edge_points(
    nodes: &BTreeMap<String, NodeLayout>,
    from: &str,
    to: &str,
) -> Option<Vec<(f32, f32)>> {
    let from_node = nodes.get(from)?;
    let to_node = nodes.get(to)?;
    Some(vec![
        (
            from_node.x + from_node.width / 2.0,
            from_node.y + from_node.height / 2.0,
        ),
        (
            to_node.x + to_node.width / 2.0,
            to_node.y + to_node.height / 2.0,
        ),
    ])
}

fn adjust_edge_endpoints_to_node_shapes(
    points: &mut [(f32, f32)],
    from_node: &NodeLayout,
    to_node: &NodeLayout,
) {
    if points.len() < 2 {
        return;
    }
    if let Some(point) = outline_intersection_toward(from_node, points[1]) {
        points[0] = point;
    }
    let last = points.len() - 1;
    if let Some(point) = outline_intersection_toward(to_node, points[last - 1]) {
        points[last] = point;
    }
}

fn outline_intersection_toward(node: &NodeLayout, toward: (f32, f32)) -> Option<(f32, f32)> {
    if node.hidden {
        return None;
    }
    let center = node_center(node);
    let dir = (toward.0 - center.0, toward.1 - center.1);
    let len2 = dir.0 * dir.0 + dir.1 * dir.1;
    if len2 <= f32::EPSILON {
        return None;
    }

    match node.shape {
        NodeShape::Circle | NodeShape::DoubleCircle => {
            ray_ellipse_intersection(center, dir, node.width / 2.0, node.height / 2.0)
        }
        NodeShape::Diamond
        | NodeShape::Hexagon
        | NodeShape::Parallelogram
        | NodeShape::ParallelogramAlt
        | NodeShape::Trapezoid
        | NodeShape::TrapezoidAlt
        | NodeShape::Asymmetric => {
            let polygon = node_polygon_points(node)?;
            ray_polygon_intersection(center, dir, &polygon)
        }
        _ => None,
    }
}

fn node_center(node: &NodeLayout) -> (f32, f32) {
    (node.x + node.width / 2.0, node.y + node.height / 2.0)
}

fn node_polygon_points(node: &NodeLayout) -> Option<Vec<(f32, f32)>> {
    let x = node.x;
    let y = node.y;
    let w = node.width;
    let h = node.height;
    match node.shape {
        NodeShape::Diamond => {
            let cx = x + w / 2.0;
            let cy = y + h / 2.0;
            Some(vec![(cx, y), (x + w, cy), (cx, y + h), (x, cy)])
        }
        NodeShape::Hexagon => {
            let x1 = x + w * 0.25;
            let x2 = x + w * 0.75;
            let y_mid = y + h / 2.0;
            Some(vec![
                (x1, y),
                (x2, y),
                (x + w, y_mid),
                (x2, y + h),
                (x1, y + h),
                (x, y_mid),
            ])
        }
        NodeShape::Parallelogram | NodeShape::ParallelogramAlt => {
            let offset = w * 0.18;
            if node.shape == NodeShape::Parallelogram {
                Some(vec![
                    (x + offset, y),
                    (x + w, y),
                    (x + w - offset, y + h),
                    (x, y + h),
                ])
            } else {
                Some(vec![
                    (x, y),
                    (x + w - offset, y),
                    (x + w, y + h),
                    (x + offset, y + h),
                ])
            }
        }
        NodeShape::Trapezoid | NodeShape::TrapezoidAlt => {
            let offset = w * 0.18;
            if node.shape == NodeShape::Trapezoid {
                Some(vec![
                    (x + offset, y),
                    (x + w - offset, y),
                    (x + w, y + h),
                    (x, y + h),
                ])
            } else {
                Some(vec![
                    (x, y),
                    (x + w, y),
                    (x + w - offset, y + h),
                    (x + offset, y + h),
                ])
            }
        }
        NodeShape::Asymmetric => {
            let slant = w * 0.22;
            Some(vec![
                (x, y),
                (x + w - slant, y),
                (x + w, y + h / 2.0),
                (x + w - slant, y + h),
                (x, y + h),
            ])
        }
        _ => None,
    }
}

fn ray_ellipse_intersection(
    origin: (f32, f32),
    dir: (f32, f32),
    rx: f32,
    ry: f32,
) -> Option<(f32, f32)> {
    if rx <= 0.0 || ry <= 0.0 {
        return None;
    }
    let denom = (dir.0 * dir.0) / (rx * rx) + (dir.1 * dir.1) / (ry * ry);
    if denom <= f32::EPSILON {
        return None;
    }
    let t = 1.0 / denom.sqrt();
    Some((origin.0 + dir.0 * t, origin.1 + dir.1 * t))
}

fn ray_polygon_intersection(
    origin: (f32, f32),
    dir: (f32, f32),
    polygon: &[(f32, f32)],
) -> Option<(f32, f32)> {
    let mut best: Option<(f32, (f32, f32))> = None;
    for idx in 0..polygon.len() {
        let a = polygon[idx];
        let b = polygon[(idx + 1) % polygon.len()];
        let edge = (b.0 - a.0, b.1 - a.1);
        let denom = cross(dir, edge);
        if denom.abs() <= f32::EPSILON {
            continue;
        }

        let rel = (a.0 - origin.0, a.1 - origin.1);
        let t = cross(rel, edge) / denom;
        let u = cross(rel, dir) / denom;
        if t >= 0.0 && (0.0..=1.0).contains(&u) {
            let point = (origin.0 + dir.0 * t, origin.1 + dir.1 * t);
            if best.is_none_or(|(best_t, _)| t < best_t) {
                best = Some((t, point));
            }
        }
    }
    best.map(|(_, point)| point)
}

fn cross(a: (f32, f32), b: (f32, f32)) -> f32 {
    a.0 * b.1 - a.1 * b.0
}
