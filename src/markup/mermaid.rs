#![allow(dead_code)]

use anyhow::{Context, Result, anyhow};
use fontdb::Database;
use image::{Rgba, RgbaImage};
use once_cell::sync::Lazy;
use resvg::{
    tiny_skia::{Pixmap, Transform},
    usvg::{Options, Tree},
};
use std::sync::Arc;

use crate::{
    markup::{
        fonts,
        image_renderer::{RenderTheme, RenderedImage, ThemeMode, rgba},
    },
    mermaid_engine,
};

const MAX_VISUAL_DIMENSION: u32 = 16_384;
const MAX_VISUAL_PIXELS: u64 = 16_700_000;

#[derive(Debug, Clone)]
pub struct MermaidRenderOptions {
    pub theme: ThemeMode,
    pub zoom: f32,
}

static SVG_FONTDB: Lazy<Arc<Database>> = Lazy::new(|| {
    let mut database = Database::new();
    database.load_system_fonts();
    fonts::load_bundled_fonts(&mut database);
    Arc::new(database)
});

impl Default for MermaidRenderOptions {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Dark,
            zoom: 2.0,
        }
    }
}

pub fn render_mermaid_png(source: &str, options: &MermaidRenderOptions) -> Result<RenderedImage> {
    let image = render_mermaid_image(source, options)?;
    RenderedImage::from_rgba_owned(image)
}

pub fn render_mermaid_image(source: &str, options: &MermaidRenderOptions) -> Result<RgbaImage> {
    let svg = render_mermaid_svg(source)?;
    let theme = RenderTheme::for_mode(options.theme);
    rasterize_svg_with_zoom(&svg, theme.background, options.zoom)
}

pub fn render_mermaid_svg(source: &str) -> Result<String> {
    let preprocessed = preprocess_mermaid(source);
    std::panic::catch_unwind(|| mermaid_engine::render(&preprocessed))
        .map_err(|_| anyhow!("Mermaid renderer panicked"))?
        .map_err(|e| anyhow!("Mermaid render error: {e}"))
}

pub fn rasterize_svg(svg_text: &str, background: Rgba<u8>) -> Result<RgbaImage> {
    rasterize_svg_with_zoom(svg_text, background, 1.0)
}

pub fn rasterize_svg_with_zoom(
    svg_text: &str,
    background: Rgba<u8>,
    zoom: f32,
) -> Result<RgbaImage> {
    let opt = Options {
        fontdb: SVG_FONTDB.clone(),
        ..Options::default()
    };

    let tree = Tree::from_str(svg_text, &opt).context("failed to parse SVG")?;
    let zoom = normalized_zoom(zoom);
    let size = tree.size();
    let width = (size.width() * zoom).ceil().max(1.0) as u32;
    let height = (size.height() * zoom).ceil().max(1.0) as u32;
    validate_raster_size(width, height)?;
    rasterize_tree(&tree, background, width, height, zoom)
}

pub fn svg_dimensions(svg_text: &str) -> Result<(u32, u32)> {
    let options = Options {
        fontdb: SVG_FONTDB.clone(),
        ..Options::default()
    };
    let tree = Tree::from_str(svg_text, &options).context("failed to parse SVG")?;
    let size = tree.size();
    let width = size.width().ceil().max(1.0) as u32;
    let height = size.height().ceil().max(1.0) as u32;
    validate_raster_size(width, height)?;
    Ok((width, height))
}

/// Rasterize directly at a bounded contain-fit size, avoiding a potentially enormous
/// intermediate pixmap for untrusted SVG or Mermaid input.
pub fn rasterize_svg_to_fit(
    svg_text: &str,
    background: Rgba<u8>,
    max_width: u32,
    max_height: u32,
) -> Result<RgbaImage> {
    let options = Options {
        fontdb: SVG_FONTDB.clone(),
        ..Options::default()
    };
    let tree = Tree::from_str(svg_text, &options).context("failed to parse SVG")?;
    let size = tree.size();
    let scale = (max_width.max(1) as f32 / size.width())
        .min(max_height.max(1) as f32 / size.height())
        .clamp(f32::EPSILON, 1.0);
    let width = (size.width() * scale).ceil().max(1.0) as u32;
    let height = (size.height() * scale).ceil().max(1.0) as u32;
    validate_raster_size(width, height)?;
    rasterize_tree(&tree, background, width, height, scale)
}

fn rasterize_tree(
    tree: &Tree,
    background: Rgba<u8>,
    width: u32,
    height: u32,
    scale: f32,
) -> Result<RgbaImage> {
    let mut pixmap =
        Pixmap::new(width, height).ok_or_else(|| anyhow!("failed to allocate SVG pixmap"))?;
    pixmap.fill(resvg::tiny_skia::Color::from_rgba8(
        background[0],
        background[1],
        background[2],
        background[3],
    ));
    let mut pixmap_mut = pixmap.as_mut();
    resvg::render(tree, Transform::from_scale(scale, scale), &mut pixmap_mut);

    let data = pixmap.take_demultiplied();
    RgbaImage::from_raw(width, height, data)
        .ok_or_else(|| anyhow!("failed to construct SVG raster image"))
}

pub fn validate_raster_size(width: u32, height: u32) -> Result<()> {
    if width == 0
        || height == 0
        || width > MAX_VISUAL_DIMENSION
        || height > MAX_VISUAL_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_VISUAL_PIXELS
    {
        return Err(anyhow!(
            "visual dimensions {width}x{height} exceed the 16384 px / 16.7 MP limit"
        ));
    }
    Ok(())
}

fn normalized_zoom(zoom: f32) -> f32 {
    if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        1.0
    }
}

fn preprocess_mermaid(source: &str) -> String {
    source
        .replace("<br/>", " ")
        .replace("<br>", " ")
        .replace("<br />", " ")
}

pub fn render_error_block(
    source: &str,
    error: &anyhow::Error,
    width: u32,
    theme: ThemeMode,
) -> RgbaImage {
    use crate::markup::image_renderer::{TextBlockOptions, TextRenderer, TextSpan, TextStyle};

    let colors = RenderTheme::for_mode(theme);
    let mut text = Vec::new();
    text.push(TextSpan {
        text: format!("Mermaid render failed: {error}\n\n"),
        style: TextStyle {
            bold: true,
            ..TextStyle::default()
        },
    });
    text.push(TextSpan {
        text: source.to_string(),
        style: TextStyle {
            code: true,
            ..TextStyle::default()
        },
    });
    TextRenderer::new().render_text_block(
        &text,
        &TextBlockOptions {
            width,
            padding_x: 14,
            padding_y: 12,
            font_size: 25.5,
            line_height: 31.5,
            background: colors.error_bg,
            default_color: colors.error_text,
            link_color: colors.link,
            code_color: colors.error_text,
            code_background: rgba(0, 0, 0, 20),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mermaid_engine::{
        Layout, NodeLayout, Theme, config::LayoutConfig, ir::NodeShape, layout::compute_layout,
        parser::parse_mermaid,
    };

    const README_HOW_IT_WORKS: &str = r#"flowchart TD
    input["Markdown or Mermaid input"] --> detect{"Input type"}
    detect -->|Markdown| markdown["Comrak GFM parser"]
    detect -->|Mermaid| mermaid["Rust Mermaid parser and layout"]

    markdown --> blocks["Markdown block renderer"]
    mermaid --> svg["Mermaid SVG renderer"]

    assets["Local images and bundled fonts"] --> blocks
    assets --> raster

    svg --> raster["resvg rasterizer"]
    blocks --> image["Raster image pipeline"]
    raster --> image

    image --> output{"Output target"}
    output -->|Terminal| kitty["Kitty graphics protocol"]
    output -->|File| png["PNG export"]
"#;

    #[test]
    fn renders_flowchart_to_svg() {
        let svg = render_mermaid_svg("flowchart LR\nA-->B").unwrap();
        assert!(svg.contains("<svg"));
    }

    #[test]
    fn flowchart_subgraph_layout_honors_parent_and_child_directions() {
        let source = r#"flowchart LR
  subgraph TOP
    direction TB
    subgraph B1
        direction RL
        i1 -->f1
    end
    subgraph B2
        direction BT
        i2 -->f2
    end
  end
  A --> TOP --> B
  B1 --> B2
"#;
        let parsed = parse_mermaid(source).unwrap();
        let layout = compute_layout(&parsed.graph, &Theme::modern(), &LayoutConfig::default());

        let top = subgraph_center(&layout, "TOP");
        let b1 = subgraph_center(&layout, "B1");
        let b2 = subgraph_center(&layout, "B2");
        let a = node_center(&layout, "A");
        let b = node_center(&layout, "B");
        let f1 = node_center(&layout, "f1");
        let i1 = node_center(&layout, "i1");
        let f2 = node_center(&layout, "f2");
        let i2 = node_center(&layout, "i2");

        assert!(
            (a.1 - top.1).abs() < 12.0,
            "A should align with TOP on LR cross-axis: A={a:?}, TOP={top:?}"
        );
        assert!(
            (b.1 - top.1).abs() < 12.0,
            "B should align with TOP on LR cross-axis: B={b:?}, TOP={top:?}"
        );
        assert!(
            b1.1 < b2.1 && (b1.0 - b2.0).abs() < 40.0,
            "TOP direction TB should stack B1 above B2: B1={b1:?}, B2={b2:?}"
        );
        assert!(
            f1.0 < i1.0 && (f1.1 - i1.1).abs() < 8.0,
            "B1 direction RL should place f1 left of i1: f1={f1:?}, i1={i1:?}"
        );
        assert!(
            f2.1 < i2.1 && (f2.0 - i2.0).abs() < 8.0,
            "B2 direction BT should place f2 above i2: f2={f2:?}, i2={i2:?}"
        );
        let b1_b2_edge = layout
            .edges
            .iter()
            .find(|edge| edge.from == "B1" && edge.to == "B2")
            .expect("B1 -> B2 edge");
        let edge_start = b1_b2_edge.points.first().copied().unwrap();
        let edge_end = b1_b2_edge.points.last().copied().unwrap();
        assert!(
            edge_start.1 < edge_end.1 && (edge_start.0 - edge_end.0).abs() < 20.0,
            "B1 -> B2 should route vertically inside TOP: start={edge_start:?}, end={edge_end:?}"
        );
    }

    #[test]
    fn renders_class_stereotype_members_to_svg() {
        let svg = render_mermaid_svg(
            "classDiagram\nclass Backend {\n    <<trait>>\n    -markdown: String\n    +run()\n}\n",
        )
        .unwrap();
        assert!(svg.contains("<svg"));
    }

    #[test]
    fn renders_sequence_to_png() {
        let rendered = render_mermaid_png(
            "sequenceDiagram\nAlice->>Bob: Hello",
            &MermaidRenderOptions::default(),
        )
        .unwrap();
        assert!(rendered.width > 0);
        assert!(rendered.height > 0);
        assert!(!rendered.png.is_empty());
    }

    #[test]
    fn readme_how_it_works_flowchart_uses_balanced_dagre_geometry() {
        let layout = readme_how_it_works_layout();
        let input = node_center(&layout, "input");
        let detect = node_center(&layout, "detect");
        let markdown = node_center(&layout, "markdown");
        let mermaid = node_center(&layout, "mermaid");
        let assets = node_center(&layout, "assets");
        let svg = node_center(&layout, "svg");
        let raster = node_center(&layout, "raster");
        let image = node_center(&layout, "image");
        let output = node_center(&layout, "output");
        let kitty = node_center(&layout, "kitty");
        let png = node_center(&layout, "png");

        assert!((input.0 - detect.0).abs() < 40.0);
        assert!(input.1 < detect.1);
        assert!(markdown.0 < detect.0);
        assert!(mermaid.0 > detect.0);
        assert!(assets.0 > markdown.0 && assets.0 < mermaid.0);
        assert!(raster.1 > svg.1);
        assert!(image.1 > raster.1);
        assert!(output.1 > image.1);
        assert!(kitty.1 > output.1);
        assert!(png.1 > output.1);
    }

    #[test]
    fn readme_how_it_works_flowchart_edges_avoid_non_endpoint_nodes() {
        let layout = readme_how_it_works_layout();
        for edge in &layout.edges {
            for segment in edge.points.windows(2) {
                let a = segment[0];
                let b = segment[1];
                for (node_id, node) in &layout.nodes {
                    if node_id == &edge.from || node_id == &edge.to || node.hidden {
                        continue;
                    }
                    assert!(
                        !segment_intersects_node_interior(a, b, node),
                        "edge {} -> {} crosses node {}",
                        edge.from,
                        edge.to,
                        node_id
                    );
                }
            }
        }
    }

    #[test]
    fn readme_how_it_works_decision_outputs_touch_diamond_outline() {
        let layout = readme_how_it_works_layout();
        let output = layout.nodes.get("output").unwrap();
        for target in ["kitty", "png"] {
            let edge = layout
                .edges
                .iter()
                .find(|edge| edge.from == "output" && edge.to == target)
                .unwrap();
            let start = edge.points.first().copied().unwrap();
            let outline_distance = diamond_outline_distance(start, output);
            assert!(
                outline_distance < 0.02,
                "edge output -> {target} should start on diamond outline, got distance {outline_distance}"
            );
        }
    }

    #[test]
    fn readme_how_it_works_flowchart_renders_nonempty_png_and_curved_svg() {
        let svg = render_mermaid_svg(README_HOW_IT_WORKS).unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains(" C "));

        let rendered = render_mermaid_png(
            README_HOW_IT_WORKS,
            &MermaidRenderOptions {
                theme: ThemeMode::Dark,
                zoom: 1.0,
            },
        )
        .unwrap();
        assert!(rendered.width > 0);
        assert!(rendered.height > 0);
        assert!(!rendered.png.is_empty());
    }

    #[test]
    fn zoom_changes_rendered_png_dimensions() {
        let source = "flowchart LR\nA-->B";
        let one = render_mermaid_png(
            source,
            &MermaidRenderOptions {
                theme: ThemeMode::Dark,
                zoom: 1.0,
            },
        )
        .unwrap();
        let two = render_mermaid_png(
            source,
            &MermaidRenderOptions {
                theme: ThemeMode::Dark,
                zoom: 2.0,
            },
        )
        .unwrap();
        assert!(two.width >= one.width.saturating_mul(2).saturating_sub(1));
        assert!(two.height >= one.height.saturating_mul(2).saturating_sub(1));
    }

    #[test]
    fn parses_v11_flowchart_datastore_shape() {
        let parsed =
            parse_mermaid("flowchart LR\nA@{ shape: datastore, label: \"Datastore\" } --> B")
                .unwrap();
        let node = parsed.graph.nodes.get("A").unwrap();
        assert_eq!(node.label, "Datastore");
        assert_eq!(node.shape, NodeShape::Cylinder);
    }

    #[test]
    fn parses_v11_sequence_decimal_autonumber() {
        let parsed = parse_mermaid("sequenceDiagram\nautonumber 1.5 0.25\nA->>B: ping").unwrap();
        let autonumber = parsed.graph.sequence_autonumber.unwrap();
        assert_eq!(autonumber.start, 1.5);
        assert_eq!(autonumber.step, 0.25);
    }

    #[test]
    fn parses_class_namespace_syntax() {
        let parsed = parse_mermaid(
            "classDiagram\nnamespace Domain {\n  class Service\n  Service : +call()\n}",
        )
        .unwrap();
        assert!(parsed.graph.nodes.contains_key("Domain.Service"));
        assert!(
            parsed.graph.nodes["Domain.Service"]
                .label
                .contains("+call()")
        );
    }

    #[test]
    fn strips_outer_quotes_from_rendered_mermaid_labels() {
        let flow = parse_mermaid("flowchart LR\nA -->|\"Edge label\"| B").unwrap();
        assert_eq!(flow.graph.edges[0].label.as_deref(), Some("Edge label"));

        let sequence =
            parse_mermaid("sequenceDiagram\nA->>B: \"quoted ping\"\nNote over A,B: 'quoted note'")
                .unwrap();
        assert_eq!(
            sequence.graph.edges[0].label.as_deref(),
            Some("quoted ping")
        );
        assert_eq!(sequence.graph.sequence_notes[0].label, "quoted note");

        let state =
            parse_mermaid("stateDiagram-v2\nstate Idle\nnote right of Idle: \"waiting\"").unwrap();
        assert_eq!(state.graph.state_notes[0].label, "waiting");
    }

    #[test]
    fn parses_quadrant_unicode_labels_without_outer_quotes() {
        let parsed = parse_mermaid(
            "quadrantChart\n  title \"增长\"\n  x-axis \"低\" --> \"高\"\n  y-axis \"慢\" --> \"快\"\n  quadrant-1 \"优先\"\n  \"活动一\" : [0.2, 0.8]",
        )
        .unwrap();
        assert_eq!(parsed.graph.quadrant.title.as_deref(), Some("增长"));
        assert_eq!(parsed.graph.quadrant.x_axis_left.as_deref(), Some("低"));
        assert_eq!(parsed.graph.quadrant.y_axis_top.as_deref(), Some("快"));
        assert_eq!(
            parsed.graph.quadrant.quadrant_labels[0].as_deref(),
            Some("优先")
        );
        assert_eq!(parsed.graph.quadrant.points[0].label, "活动一");
    }

    fn readme_how_it_works_layout() -> Layout {
        let parsed = parse_mermaid(README_HOW_IT_WORKS).unwrap();
        compute_layout(&parsed.graph, &Theme::modern(), &LayoutConfig::default())
    }

    fn node_center(layout: &Layout, node_id: &str) -> (f32, f32) {
        let node = layout.nodes.get(node_id).unwrap();
        (node.x + node.width / 2.0, node.y + node.height / 2.0)
    }

    fn subgraph_center(layout: &Layout, label: &str) -> (f32, f32) {
        let subgraph = layout
            .subgraphs
            .iter()
            .find(|subgraph| subgraph.label == label)
            .unwrap();
        (
            subgraph.x + subgraph.width / 2.0,
            subgraph.y + subgraph.height / 2.0,
        )
    }

    fn diamond_outline_distance(point: (f32, f32), node: &NodeLayout) -> f32 {
        let center = (node.x + node.width / 2.0, node.y + node.height / 2.0);
        let normalized = (point.0 - center.0).abs() / (node.width / 2.0)
            + (point.1 - center.1).abs() / (node.height / 2.0);
        (normalized - 1.0).abs()
    }

    fn segment_intersects_node_interior(a: (f32, f32), b: (f32, f32), node: &NodeLayout) -> bool {
        let pad = 1.0;
        let x1 = node.x + pad;
        let y1 = node.y + pad;
        let x2 = node.x + node.width - pad;
        let y2 = node.y + node.height - pad;
        if x2 <= x1 || y2 <= y1 {
            return false;
        }

        if point_in_rect(a, x1, y1, x2, y2) || point_in_rect(b, x1, y1, x2, y2) {
            return true;
        }

        segments_intersect(a, b, (x1, y1), (x2, y1))
            || segments_intersect(a, b, (x2, y1), (x2, y2))
            || segments_intersect(a, b, (x2, y2), (x1, y2))
            || segments_intersect(a, b, (x1, y2), (x1, y1))
    }

    fn point_in_rect(point: (f32, f32), x1: f32, y1: f32, x2: f32, y2: f32) -> bool {
        point.0 > x1 && point.0 < x2 && point.1 > y1 && point.1 < y2
    }

    fn segments_intersect(a: (f32, f32), b: (f32, f32), c: (f32, f32), d: (f32, f32)) -> bool {
        let o1 = orient(a, b, c);
        let o2 = orient(a, b, d);
        let o3 = orient(c, d, a);
        let o4 = orient(c, d, b);
        if o1.abs() < f32::EPSILON && on_segment(a, c, b) {
            return true;
        }
        if o2.abs() < f32::EPSILON && on_segment(a, d, b) {
            return true;
        }
        if o3.abs() < f32::EPSILON && on_segment(c, a, d) {
            return true;
        }
        if o4.abs() < f32::EPSILON && on_segment(c, b, d) {
            return true;
        }
        (o1 > 0.0) != (o2 > 0.0) && (o3 > 0.0) != (o4 > 0.0)
    }

    fn orient(a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> f32 {
        (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
    }

    fn on_segment(a: (f32, f32), p: (f32, f32), b: (f32, f32)) -> bool {
        p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1)
    }
}
