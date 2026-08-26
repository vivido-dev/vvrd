use std::collections::HashSet;
use std::fmt;

use crate::mermaid_engine::ir::DiagramKind;

use super::{C4TextLayout, DiagramData, Layout, TextBlock};

const EPS: f32 = 0.1;
const FLOWCHART_LABEL_ROUTE_CLEARANCE: f32 = 0.0;
const SEQUENCE_LABEL_LIFELINE_PAD_X: f32 = 4.0;
const SEQUENCE_LABEL_LIFELINE_PAD_Y: f32 = 2.0;

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutInvariantError {
    pub path: String,
    pub message: String,
}

impl LayoutInvariantError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for LayoutInvariantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Validate cross-cutting layout invariants that renderers rely on.
///
/// This is intentionally independent from tests so parser, CLI, fuzzers, and
/// future fallible layout APIs can share the same contract checks.
pub fn validate_layout_invariants(layout: &Layout) -> Result<(), Vec<LayoutInvariantError>> {
    let mut errors = Vec::new();

    check_finite_positive(&mut errors, "layout.width", layout.width);
    check_finite_positive(&mut errors, "layout.height", layout.height);

    for (id, node) in &layout.nodes {
        let path = format!("nodes[{id}]");
        check_rect(&mut errors, &path, node.x, node.y, node.width, node.height);
        check_text_block(&mut errors, &format!("{path}.label"), &node.label);
        if !node.hidden {
            check_inside_layout(
                &mut errors,
                &path,
                node.x,
                node.y,
                node.width,
                node.height,
                layout,
            );
        }
    }

    for (idx, subgraph) in layout.subgraphs.iter().enumerate() {
        let path = format!("subgraphs[{idx}:{}]", subgraph.label);
        check_rect(
            &mut errors,
            &path,
            subgraph.x,
            subgraph.y,
            subgraph.width,
            subgraph.height,
        );
        check_text_block(&mut errors, &format!("{path}.label"), &subgraph.label_block);
        check_inside_layout(
            &mut errors,
            &path,
            subgraph.x,
            subgraph.y,
            subgraph.width,
            subgraph.height,
            layout,
        );
    }

    for (idx, edge) in layout.edges.iter().enumerate() {
        let path = format!("edges[{idx}:{}->{}]", edge.from, edge.to);
        if edge.points.len() < 2 {
            errors.push(LayoutInvariantError::new(
                format!("{path}.points"),
                "must contain at least two points",
            ));
        }
        for (point_idx, (x, y)) in edge.points.iter().copied().enumerate() {
            check_point(&mut errors, &format!("{path}.points[{point_idx}]"), (x, y));
        }
        if let Some(label) = &edge.label {
            check_text_block(&mut errors, &format!("{path}.label"), label);
            check_anchor(
                &mut errors,
                &format!("{path}.label_anchor"),
                edge.label_anchor,
            );
        }
        if let Some(label) = &edge.start_label {
            check_text_block(&mut errors, &format!("{path}.start_label"), label);
            check_anchor(
                &mut errors,
                &format!("{path}.start_label_anchor"),
                edge.start_label_anchor,
            );
        }
        if let Some(label) = &edge.end_label {
            check_text_block(&mut errors, &format!("{path}.end_label"), label);
            check_anchor(
                &mut errors,
                &format!("{path}.end_label_anchor"),
                edge.end_label_anchor,
            );
        }
    }

    match &layout.diagram {
        DiagramData::Graph { state_notes } => {
            for (idx, note) in state_notes.iter().enumerate() {
                let path = format!("state_notes[{idx}]");
                check_rect(&mut errors, &path, note.x, note.y, note.width, note.height);
                check_text_block(&mut errors, &format!("{path}.label"), &note.label);
            }
        }
        DiagramData::Sequence(seq) => {
            for lifeline in &seq.lifelines {
                check_finite(
                    &mut errors,
                    &format!("lifelines[{}].x", lifeline.id),
                    lifeline.x,
                );
                check_finite(
                    &mut errors,
                    &format!("lifelines[{}].y1", lifeline.id),
                    lifeline.y1,
                );
                check_finite(
                    &mut errors,
                    &format!("lifelines[{}].y2", lifeline.id),
                    lifeline.y2,
                );
            }
            for (idx, footbox) in seq.footboxes.iter().enumerate() {
                let path = format!("sequence.footboxes[{idx}:{}]", footbox.id);
                check_rect(
                    &mut errors,
                    &path,
                    footbox.x,
                    footbox.y,
                    footbox.width,
                    footbox.height,
                );
                check_text_block(&mut errors, &format!("{path}.label"), &footbox.label);
            }
            for (idx, b) in seq.boxes.iter().enumerate() {
                let path = format!("sequence.boxes[{idx}]");
                check_rect(&mut errors, &path, b.x, b.y, b.width, b.height);
                if let Some(label) = &b.label {
                    check_text_block(&mut errors, &format!("{path}.label"), label);
                }
            }
            for (idx, frame) in seq.frames.iter().enumerate() {
                let path = format!("sequence.frames[{idx}]");
                check_rect(
                    &mut errors,
                    &path,
                    frame.x,
                    frame.y,
                    frame.width,
                    frame.height,
                );
                check_rect_tuple(&mut errors, &format!("{path}.label_box"), frame.label_box);
                check_sequence_label(&mut errors, &format!("{path}.label"), &frame.label);
                for (section_idx, label) in frame.section_labels.iter().enumerate() {
                    check_sequence_label(
                        &mut errors,
                        &format!("{path}.section_labels[{section_idx}]"),
                        label,
                    );
                }
                for (divider_idx, divider) in frame.dividers.iter().copied().enumerate() {
                    check_finite(
                        &mut errors,
                        &format!("{path}.dividers[{divider_idx}]"),
                        divider,
                    );
                }
            }
            for (idx, note) in seq.notes.iter().enumerate() {
                let path = format!("sequence.notes[{idx}]");
                check_rect(&mut errors, &path, note.x, note.y, note.width, note.height);
                check_text_block(&mut errors, &format!("{path}.label"), &note.label);
            }
            for (idx, activation) in seq.activations.iter().enumerate() {
                check_rect(
                    &mut errors,
                    &format!("sequence.activations[{idx}:{}]", activation.participant),
                    activation.x,
                    activation.y,
                    activation.width,
                    activation.height,
                );
            }
            for (idx, number) in seq.numbers.iter().enumerate() {
                check_point(
                    &mut errors,
                    &format!("sequence.numbers[{idx}]"),
                    (number.x, number.y),
                );
            }
        }
        DiagramData::Pie(pie) => {
            check_point(&mut errors, "pie.center", pie.center);
            check_finite_positive(&mut errors, "pie.radius", pie.radius);
            for (idx, slice) in pie.slices.iter().enumerate() {
                let path = format!("pie.slices[{idx}]");
                check_text_block(&mut errors, &format!("{path}.label"), &slice.label);
                check_finite(&mut errors, &format!("{path}.value"), slice.value);
                check_finite(
                    &mut errors,
                    &format!("{path}.start_angle"),
                    slice.start_angle,
                );
                check_finite(&mut errors, &format!("{path}.end_angle"), slice.end_angle);
            }
            for (idx, item) in pie.legend.iter().enumerate() {
                let path = format!("pie.legend[{idx}]");
                check_point(&mut errors, &path, (item.x, item.y));
                check_text_block(&mut errors, &format!("{path}.label"), &item.label);
                check_finite(
                    &mut errors,
                    &format!("{path}.marker_size"),
                    item.marker_size,
                );
                check_finite(&mut errors, &format!("{path}.value"), item.value);
            }
            if let Some(title) = &pie.title {
                check_point(&mut errors, "pie.title", (title.x, title.y));
                check_text_block(&mut errors, "pie.title.text", &title.text);
            }
        }
        DiagramData::Quadrant(quad) => {
            check_finite(&mut errors, "quadrant.title_y", quad.title_y);
            for (path, label) in [
                ("quadrant.title", quad.title.as_ref()),
                ("quadrant.x_axis_left", quad.x_axis_left.as_ref()),
                ("quadrant.x_axis_right", quad.x_axis_right.as_ref()),
                ("quadrant.y_axis_bottom", quad.y_axis_bottom.as_ref()),
                ("quadrant.y_axis_top", quad.y_axis_top.as_ref()),
            ] {
                if let Some(label) = label {
                    check_text_block(&mut errors, path, label);
                }
            }
            for (idx, label) in quad.quadrant_labels.iter().enumerate() {
                if let Some(label) = label {
                    check_text_block(
                        &mut errors,
                        &format!("quadrant.quadrant_labels[{idx}]"),
                        label,
                    );
                }
            }
            check_rect(
                &mut errors,
                "quadrant.grid",
                quad.grid_x,
                quad.grid_y,
                quad.grid_width,
                quad.grid_height,
            );
            for (idx, point) in quad.points.iter().enumerate() {
                let path = format!("quadrant.points[{idx}]");
                check_point(&mut errors, &path, (point.x, point.y));
                check_text_block(&mut errors, &format!("{path}.label"), &point.label);
            }
        }
        DiagramData::Gantt(gantt) => {
            if let Some(title) = &gantt.title {
                check_text_block(&mut errors, "gantt.title", title);
            }
            for (path, value) in [
                ("gantt.time_start", gantt.time_start),
                ("gantt.time_end", gantt.time_end),
                ("gantt.chart_x", gantt.chart_x),
                ("gantt.chart_y", gantt.chart_y),
                ("gantt.chart_width", gantt.chart_width),
                ("gantt.chart_height", gantt.chart_height),
                ("gantt.row_height", gantt.row_height),
                ("gantt.label_x", gantt.label_x),
                ("gantt.label_width", gantt.label_width),
                ("gantt.section_label_x", gantt.section_label_x),
                ("gantt.section_label_width", gantt.section_label_width),
                ("gantt.task_label_x", gantt.task_label_x),
                ("gantt.task_label_width", gantt.task_label_width),
                ("gantt.title_y", gantt.title_y),
            ] {
                check_finite(&mut errors, path, value);
            }
            for (idx, section) in gantt.sections.iter().enumerate() {
                let path = format!("gantt.sections[{idx}]");
                check_rect(&mut errors, &path, 0.0, section.y, 0.0, section.height);
                check_text_block(&mut errors, &format!("{path}.label"), &section.label);
            }
            for (idx, task) in gantt.tasks.iter().enumerate() {
                let path = format!("gantt.tasks[{idx}]");
                check_rect(&mut errors, &path, task.x, task.y, task.width, task.height);
                check_text_block(&mut errors, &format!("{path}.label"), &task.label);
                check_finite(&mut errors, &format!("{path}.start"), task.start);
                check_finite(&mut errors, &format!("{path}.duration"), task.duration);
            }
            for (idx, tick) in gantt.ticks.iter().enumerate() {
                check_finite(&mut errors, &format!("gantt.ticks[{idx}].x"), tick.x);
            }
        }
        DiagramData::Sankey(sankey) => {
            check_finite_positive(&mut errors, "sankey.width", sankey.width);
            check_finite_positive(&mut errors, "sankey.height", sankey.height);
            check_finite(&mut errors, "sankey.node_width", sankey.node_width);
            for (idx, node) in sankey.nodes.iter().enumerate() {
                let path = format!("sankey.nodes[{idx}:{}]", node.id);
                check_rect(&mut errors, &path, node.x, node.y, node.width, node.height);
                check_finite(&mut errors, &format!("{path}.total"), node.total);
            }
            for (idx, link) in sankey.links.iter().enumerate() {
                let path = format!("sankey.links[{idx}:{}->{}]", link.source, link.target);
                check_finite(&mut errors, &format!("{path}.value"), link.value);
                check_finite(&mut errors, &format!("{path}.thickness"), link.thickness);
                check_point(&mut errors, &format!("{path}.start"), link.start);
                check_point(&mut errors, &format!("{path}.end"), link.end);
            }
        }
        DiagramData::GitGraph(git) => {
            for (path, value) in [
                ("gitgraph.width", git.width),
                ("gitgraph.height", git.height),
                ("gitgraph.offset_x", git.offset_x),
                ("gitgraph.offset_y", git.offset_y),
                ("gitgraph.max_pos", git.max_pos),
            ] {
                check_finite(&mut errors, path, value);
            }
            for (idx, branch) in git.branches.iter().enumerate() {
                let path = format!("gitgraph.branches[{idx}:{}]", branch.name);
                check_finite(&mut errors, &format!("{path}.pos"), branch.pos);
                check_rect(
                    &mut errors,
                    &format!("{path}.label.bg"),
                    branch.label.bg_x,
                    branch.label.bg_y,
                    branch.label.bg_width,
                    branch.label.bg_height,
                );
                check_rect(
                    &mut errors,
                    &format!("{path}.label.text"),
                    branch.label.text_x,
                    branch.label.text_y,
                    branch.label.text_width,
                    branch.label.text_height,
                );
            }
            for (idx, commit) in git.commits.iter().enumerate() {
                let path = format!("gitgraph.commits[{idx}:{}]", commit.id);
                for (field, value) in [
                    ("x", commit.x),
                    ("y", commit.y),
                    ("axis_pos", commit.axis_pos),
                ] {
                    check_finite(&mut errors, &format!("{path}.{field}"), value);
                }
                if let Some(label) = &commit.label {
                    check_rect(
                        &mut errors,
                        &format!("{path}.label.bg"),
                        label.bg_x,
                        label.bg_y,
                        label.bg_width,
                        label.bg_height,
                    );
                    check_point(
                        &mut errors,
                        &format!("{path}.label.text"),
                        (label.text_x, label.text_y),
                    );
                    check_git_transform(
                        &mut errors,
                        &format!("{path}.label.transform"),
                        label.transform.as_ref(),
                    );
                }
                for (tag_idx, tag) in commit.tags.iter().enumerate() {
                    let tag_path = format!("{path}.tags[{tag_idx}]");
                    check_point(
                        &mut errors,
                        &format!("{tag_path}.text"),
                        (tag.text_x, tag.text_y),
                    );
                    check_point(
                        &mut errors,
                        &format!("{tag_path}.hole"),
                        (tag.hole_x, tag.hole_y),
                    );
                    for (point_idx, point) in tag.points.iter().copied().enumerate() {
                        check_point(
                            &mut errors,
                            &format!("{tag_path}.points[{point_idx}]"),
                            point,
                        );
                    }
                    check_git_transform(
                        &mut errors,
                        &format!("{tag_path}.transform"),
                        tag.transform.as_ref(),
                    );
                }
            }
        }
        DiagramData::C4(c4) => {
            check_rect(
                &mut errors,
                "c4.viewbox",
                c4.viewbox_x,
                c4.viewbox_y,
                c4.viewbox_width,
                c4.viewbox_height,
            );
            for (idx, shape) in c4.shapes.iter().enumerate() {
                let path = format!("c4.shapes[{idx}:{}]", shape.id);
                check_rect(
                    &mut errors,
                    &path,
                    shape.x,
                    shape.y,
                    shape.width,
                    shape.height,
                );
                check_finite(&mut errors, &format!("{path}.margin"), shape.margin);
                check_c4_text(
                    &mut errors,
                    &format!("{path}.type_label"),
                    &shape.type_label,
                );
                check_c4_text(&mut errors, &format!("{path}.label"), &shape.label);
                if let Some(text) = &shape.type_or_techn {
                    check_c4_text(&mut errors, &format!("{path}.type_or_techn"), text);
                }
                if let Some(text) = &shape.descr {
                    check_c4_text(&mut errors, &format!("{path}.descr"), text);
                }
                if let Some(image_y) = shape.image_y {
                    check_finite(&mut errors, &format!("{path}.image_y"), image_y);
                }
            }
            for (idx, boundary) in c4.boundaries.iter().enumerate() {
                let path = format!("c4.boundaries[{idx}:{}]", boundary.id);
                check_rect(
                    &mut errors,
                    &path,
                    boundary.x,
                    boundary.y,
                    boundary.width,
                    boundary.height,
                );
                check_c4_text(&mut errors, &format!("{path}.label"), &boundary.label);
                if let Some(text) = &boundary.boundary_type {
                    check_c4_text(&mut errors, &format!("{path}.boundary_type"), text);
                }
                if let Some(text) = &boundary.descr {
                    check_c4_text(&mut errors, &format!("{path}.descr"), text);
                }
            }
            for (idx, rel) in c4.rels.iter().enumerate() {
                let path = format!("c4.rels[{idx}:{}->{}]", rel.from, rel.to);
                check_c4_text(&mut errors, &format!("{path}.label"), &rel.label);
                if let Some(text) = &rel.techn {
                    check_c4_text(&mut errors, &format!("{path}.techn"), text);
                }
                check_point(&mut errors, &format!("{path}.start"), rel.start);
                check_point(&mut errors, &format!("{path}.end"), rel.end);
                check_point(
                    &mut errors,
                    &format!("{path}.offset"),
                    (rel.offset_x, rel.offset_y),
                );
            }
        }
        DiagramData::XYChart(xy) => {
            for (path, label) in [
                ("xy.title", xy.title.as_ref()),
                ("xy.x_axis_label", xy.x_axis_label.as_ref()),
                ("xy.y_axis_label", xy.y_axis_label.as_ref()),
            ] {
                if let Some(label) = label {
                    check_text_block(&mut errors, path, label);
                }
            }
            for (path, value) in [
                ("xy.title_y", xy.title_y),
                ("xy.x_axis_label_y", xy.x_axis_label_y),
                ("xy.y_axis_label_x", xy.y_axis_label_x),
            ] {
                check_finite(&mut errors, path, value);
            }
            check_rect(
                &mut errors,
                "xy.plot",
                xy.plot_x,
                xy.plot_y,
                xy.plot_width,
                xy.plot_height,
            );
            check_finite_positive(&mut errors, "xy.width", xy.width);
            check_finite_positive(&mut errors, "xy.height", xy.height);
            for (idx, (_, x)) in xy.x_axis_categories.iter().enumerate() {
                check_finite(&mut errors, &format!("xy.x_axis_categories[{idx}].x"), *x);
            }
            for (idx, (_, y)) in xy.y_axis_ticks.iter().enumerate() {
                check_finite(&mut errors, &format!("xy.y_axis_ticks[{idx}].y"), *y);
            }
            for (idx, bar) in xy.bars.iter().enumerate() {
                let path = format!("xy.bars[{idx}]");
                check_rect(&mut errors, &path, bar.x, bar.y, bar.width, bar.height);
                check_finite(&mut errors, &format!("{path}.value"), bar.value);
            }
            for (idx, line) in xy.lines.iter().enumerate() {
                for (point_idx, point) in line.points.iter().copied().enumerate() {
                    check_point(
                        &mut errors,
                        &format!("xy.lines[{idx}].points[{point_idx}]"),
                        point,
                    );
                }
            }
        }
        DiagramData::Timeline(tl) => {
            if let Some(title) = &tl.title {
                check_text_block(&mut errors, "timeline.title", title);
            }
            for (path, value) in [
                ("timeline.title_y", tl.title_y),
                ("timeline.line_y", tl.line_y),
                ("timeline.line_start_x", tl.line_start_x),
                ("timeline.line_end_x", tl.line_end_x),
                ("timeline.line_start_y", tl.line_start_y),
                ("timeline.line_end_y", tl.line_end_y),
            ] {
                check_finite(&mut errors, path, value);
            }
            check_finite_positive(&mut errors, "timeline.width", tl.width);
            check_finite_positive(&mut errors, "timeline.height", tl.height);
            for (idx, event) in tl.events.iter().enumerate() {
                let path = format!("timeline.events[{idx}]");
                check_rect(
                    &mut errors,
                    &path,
                    event.x,
                    event.y,
                    event.width,
                    event.height,
                );
                check_finite(&mut errors, &format!("{path}.circle_y"), event.circle_y);
                check_text_block(&mut errors, &format!("{path}.time"), &event.time);
                for (event_idx, text) in event.events.iter().enumerate() {
                    check_text_block(&mut errors, &format!("{path}.events[{event_idx}]"), text);
                }
            }
            for (idx, section) in tl.sections.iter().enumerate() {
                let path = format!("timeline.sections[{idx}]");
                check_rect(
                    &mut errors,
                    &path,
                    section.x,
                    section.y,
                    section.width,
                    section.height,
                );
                check_text_block(&mut errors, &format!("{path}.label"), &section.label);
            }
        }
        DiagramData::Journey(journey) => {
            if let Some(title) = &journey.title {
                check_text_block(&mut errors, "journey.title", title);
            }
            for (path, value) in [
                ("journey.title_y", journey.title_y),
                ("journey.actor_label_y", journey.actor_label_y),
                ("journey.score_radius", journey.score_radius),
                ("journey.actor_radius", journey.actor_radius),
                ("journey.actor_gap", journey.actor_gap),
                ("journey.card_gap_y", journey.card_gap_y),
            ] {
                check_finite(&mut errors, path, value);
            }
            check_finite_positive(&mut errors, "journey.width", journey.width);
            check_finite_positive(&mut errors, "journey.height", journey.height);
            if let Some((x1, y, x2)) = journey.baseline {
                check_point(&mut errors, "journey.baseline.start", (x1, y));
                check_finite(&mut errors, "journey.baseline.x2", x2);
            }
            for (idx, actor) in journey.actors.iter().enumerate() {
                let path = format!("journey.actors[{idx}:{}]", actor.name);
                check_point(&mut errors, &path, (actor.x, actor.y));
                check_finite(&mut errors, &format!("{path}.radius"), actor.radius);
            }
            for (idx, task) in journey.tasks.iter().enumerate() {
                let path = format!("journey.tasks[{idx}:{}]", task.id);
                check_rect(&mut errors, &path, task.x, task.y, task.width, task.height);
                check_text_block(&mut errors, &format!("{path}.label"), &task.label);
                if let Some(score) = task.score {
                    check_finite(&mut errors, &format!("{path}.score"), score);
                }
                check_finite(&mut errors, &format!("{path}.score_y"), task.score_y);
                if let Some(actor_y) = task.actor_y {
                    check_finite(&mut errors, &format!("{path}.actor_y"), actor_y);
                }
            }
            for (idx, section) in journey.sections.iter().enumerate() {
                let path = format!("journey.sections[{idx}]");
                check_rect(
                    &mut errors,
                    &path,
                    section.x,
                    section.y,
                    section.width,
                    section.height,
                );
                check_text_block(&mut errors, &format!("{path}.label"), &section.label);
            }
        }
        DiagramData::Error(error) => {
            for (path, value) in [
                ("error.viewbox_width", error.viewbox_width),
                ("error.viewbox_height", error.viewbox_height),
                ("error.render_width", error.render_width),
                ("error.render_height", error.render_height),
                ("error.text_x", error.text_x),
                ("error.text_y", error.text_y),
                ("error.text_size", error.text_size),
                ("error.version_x", error.version_x),
                ("error.version_y", error.version_y),
                ("error.version_size", error.version_size),
                ("error.icon_scale", error.icon_scale),
                ("error.icon_tx", error.icon_tx),
                ("error.icon_ty", error.icon_ty),
            ] {
                check_finite(&mut errors, path, value);
            }
        }
    }

    if layout.kind == DiagramKind::Flowchart {
        validate_flowchart_invariants(layout, &mut errors);
    }
    validate_sequence_invariants(layout, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_flowchart_invariants(layout: &Layout, errors: &mut Vec<LayoutInvariantError>) {
    for (idx, left) in layout.subgraphs.iter().enumerate() {
        let left_nodes: HashSet<&str> = left.nodes.iter().map(String::as_str).collect();
        for right in layout.subgraphs.iter().skip(idx + 1) {
            if right
                .nodes
                .iter()
                .any(|node| left_nodes.contains(node.as_str()))
            {
                continue;
            }
            if rects_overlap(
                (left.x, left.y, left.width, left.height),
                (right.x, right.y, right.width, right.height),
            ) {
                errors.push(LayoutInvariantError::new(
                    "subgraphs",
                    format!(
                        "flowchart subgraphs '{}' and '{}' overlap",
                        left.label, right.label
                    ),
                ));
            }
        }
    }

    for (edge_idx, edge) in layout.edges.iter().enumerate() {
        let edge_path = format!("edges[{edge_idx}:{}->{}]", edge.from, edge.to);
        if let (Some(label), Some(anchor)) = (&edge.label, edge.label_anchor) {
            let rect = centered_text_rect(anchor, label, FLOWCHART_LABEL_ROUTE_CLEARANCE);
            if path_intersects_rect(&edge.points, rect) {
                errors.push(LayoutInvariantError::new(
                    format!("{edge_path}.label"),
                    "route overlaps its own center label box",
                ));
            }
        }

        for (node_id, node) in &layout.nodes {
            if node.hidden || node_id == &edge.from || node_id == &edge.to {
                continue;
            }
            let rect = (node.x, node.y, node.width, node.height);
            if path_intersects_rect(&edge.points, rect) {
                errors.push(LayoutInvariantError::new(
                    edge_path.clone(),
                    format!("route intersects non-endpoint node '{node_id}'"),
                ));
            }
        }
    }
}

fn validate_sequence_invariants(layout: &Layout, errors: &mut Vec<LayoutInvariantError>) {
    let DiagramData::Sequence(seq) = &layout.diagram else {
        return;
    };
    for (edge_idx, edge) in layout.edges.iter().enumerate() {
        let (Some(label), Some(anchor)) = (&edge.label, edge.label_anchor) else {
            continue;
        };
        let label_rect = centered_text_rect(
            anchor,
            label,
            SEQUENCE_LABEL_LIFELINE_PAD_X.max(SEQUENCE_LABEL_LIFELINE_PAD_Y),
        );
        for lifeline in &seq.lifelines {
            if lifeline.id == edge.from || lifeline.id == edge.to {
                continue;
            }
            let line_rect = (
                lifeline.x - 1.5,
                lifeline.y1,
                3.0,
                lifeline.y2 - lifeline.y1,
            );
            if rects_overlap(label_rect, line_rect) {
                errors.push(LayoutInvariantError::new(
                    format!("edges[{edge_idx}:{}->{}].label", edge.from, edge.to),
                    format!("sequence label overlaps lifeline '{}'", lifeline.id),
                ));
            }
        }
    }
}

fn check_rect(
    errors: &mut Vec<LayoutInvariantError>,
    path: &str,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
) {
    check_finite(errors, &format!("{path}.x"), x);
    check_finite(errors, &format!("{path}.y"), y);
    check_finite(errors, &format!("{path}.width"), width);
    check_finite(errors, &format!("{path}.height"), height);
    if width < 0.0 {
        errors.push(LayoutInvariantError::new(
            format!("{path}.width"),
            "must be non-negative",
        ));
    }
    if height < 0.0 {
        errors.push(LayoutInvariantError::new(
            format!("{path}.height"),
            "must be non-negative",
        ));
    }
}

fn check_rect_tuple(
    errors: &mut Vec<LayoutInvariantError>,
    path: &str,
    rect: (f32, f32, f32, f32),
) {
    check_rect(errors, path, rect.0, rect.1, rect.2, rect.3);
}

fn check_point(errors: &mut Vec<LayoutInvariantError>, path: &str, point: (f32, f32)) {
    check_finite(errors, &format!("{path}.x"), point.0);
    check_finite(errors, &format!("{path}.y"), point.1);
}

fn check_sequence_label(
    errors: &mut Vec<LayoutInvariantError>,
    path: &str,
    label: &super::SequenceLabel,
) {
    check_point(errors, path, (label.x, label.y));
    check_text_block(errors, &format!("{path}.text"), &label.text);
}

fn check_git_transform(
    errors: &mut Vec<LayoutInvariantError>,
    path: &str,
    transform: Option<&super::GitGraphTransform>,
) {
    if let Some(transform) = transform {
        for (field, value) in [
            ("translate_x", transform.translate_x),
            ("translate_y", transform.translate_y),
            ("rotate_deg", transform.rotate_deg),
            ("rotate_cx", transform.rotate_cx),
            ("rotate_cy", transform.rotate_cy),
        ] {
            check_finite(errors, &format!("{path}.{field}"), value);
        }
    }
}

fn check_c4_text(errors: &mut Vec<LayoutInvariantError>, path: &str, text: &C4TextLayout) {
    check_finite(errors, &format!("{path}.width"), text.width);
    check_finite(errors, &format!("{path}.height"), text.height);
    check_finite(errors, &format!("{path}.y"), text.y);
    if text.width < 0.0 {
        errors.push(LayoutInvariantError::new(
            format!("{path}.width"),
            "must be non-negative",
        ));
    }
    if text.height < 0.0 {
        errors.push(LayoutInvariantError::new(
            format!("{path}.height"),
            "must be non-negative",
        ));
    }
}

fn check_inside_layout(
    errors: &mut Vec<LayoutInvariantError>,
    path: &str,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    layout: &Layout,
) {
    if x < -EPS || y < -EPS || x + width > layout.width + EPS || y + height > layout.height + EPS {
        errors.push(LayoutInvariantError::new(
            path,
            "rectangle exceeds layout bounds",
        ));
    }
}

fn check_text_block(errors: &mut Vec<LayoutInvariantError>, path: &str, text: &TextBlock) {
    check_finite(errors, &format!("{path}.width"), text.width);
    check_finite(errors, &format!("{path}.height"), text.height);
    if text.width < 0.0 {
        errors.push(LayoutInvariantError::new(
            format!("{path}.width"),
            "must be non-negative",
        ));
    }
    if text.height < 0.0 {
        errors.push(LayoutInvariantError::new(
            format!("{path}.height"),
            "must be non-negative",
        ));
    }
}

fn check_anchor(errors: &mut Vec<LayoutInvariantError>, path: &str, anchor: Option<(f32, f32)>) {
    match anchor {
        Some((x, y)) => {
            check_finite(errors, &format!("{path}.x"), x);
            check_finite(errors, &format!("{path}.y"), y);
        }
        None => errors.push(LayoutInvariantError::new(
            path,
            "label is missing its anchor",
        )),
    }
}

fn check_finite_positive(errors: &mut Vec<LayoutInvariantError>, path: &str, value: f32) {
    check_finite(errors, path, value);
    if value <= 0.0 {
        errors.push(LayoutInvariantError::new(path, "must be positive"));
    }
}

fn check_finite(errors: &mut Vec<LayoutInvariantError>, path: &str, value: f32) {
    if !value.is_finite() {
        errors.push(LayoutInvariantError::new(path, "must be finite"));
    }
}

fn centered_text_rect(anchor: (f32, f32), text: &TextBlock, pad: f32) -> (f32, f32, f32, f32) {
    (
        anchor.0 - text.width / 2.0 - pad,
        anchor.1 - text.height / 2.0 - pad,
        text.width + pad * 2.0,
        text.height + pad * 2.0,
    )
}

fn path_intersects_rect(points: &[(f32, f32)], rect: (f32, f32, f32, f32)) -> bool {
    points
        .windows(2)
        .any(|segment| segment_intersects_rect(segment[0], segment[1], rect))
}

fn rects_overlap(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

fn segment_intersects_rect(a: (f32, f32), b: (f32, f32), rect: (f32, f32, f32, f32)) -> bool {
    let (rx, ry, rw, rh) = rect;
    if rw <= 0.0 || rh <= 0.0 {
        return false;
    }
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let p = [-dx, dx, -dy, dy];
    let q = [a.0 - rx, rx + rw - a.0, a.1 - ry, ry + rh - a.1];
    let mut u1 = 0.0f32;
    let mut u2 = 1.0f32;

    for (pi, qi) in p.into_iter().zip(q) {
        if pi.abs() <= f32::EPSILON {
            if qi < 0.0 {
                return false;
            }
            continue;
        }
        let t = qi / pi;
        if pi < 0.0 {
            if t > u2 {
                return false;
            }
            if t > u1 {
                u1 = t;
            }
        } else {
            if t < u1 {
                return false;
            }
            if t < u2 {
                u2 = t;
            }
        }
    }

    true
}
