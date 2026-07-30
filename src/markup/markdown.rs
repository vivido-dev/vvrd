//! Fixed Letter-page Markdown and standalone Mermaid document backend.
//!
//! The parser deliberately owns everything copied out of Comrak's arena. `PagePlan` stores only
//! block indices, unit ranges, and semantic geometry; page bitmaps are allocated lazily.

use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    ops::Range,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, ensure};
use base64::Engine as _;
use comrak::{
    Arena, Options,
    nodes::{AstNode, ListType, NodeValue, TableAlignment},
    parse_document,
};
use image::{ImageReader, Rgba, RgbaImage, imageops::FilterType};

use crate::{
    markup::{
        ThemeMode,
        image_renderer::{
            Canvas, RenderTheme, TextBlockOptions, TextRenderer, TextSpan, TextStyle,
        },
        mermaid::{
            rasterize_svg_to_fit, render_error_block, render_mermaid_svg, svg_dimensions,
            validate_raster_size,
        },
    },
    renderer::{LinkInfo, TocEntry},
};

pub const PAGE_WIDTH: u32 = 2_040;
pub const PAGE_HEIGHT: u32 = 2_640;
pub const PAGE_MARGIN: u32 = 180;
pub const CONTENT_WIDTH: u32 = PAGE_WIDTH - PAGE_MARGIN * 2;
pub const CONTENT_HEIGHT: u32 = PAGE_HEIGHT - PAGE_MARGIN * 2;

pub const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ASSET_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_BLOCKS: usize = 100_000;
pub const MAX_PAGES: usize = 10_000;

const BLOCK_GAP: u32 = 16;
const PROSE_FONT_SIZE: f32 = 30.0;
const PROSE_LINE_HEIGHT: f32 = 42.0;
const CODE_FONT_SIZE: f32 = 27.0;
const CODE_LINE_HEIGHT: f32 = 38.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkupKind {
    Markdown,
    Mermaid,
}

impl MarkupKind {
    pub fn format_name(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Mermaid => "Mermaid",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MarkupDocument {
    kind: MarkupKind,
    theme_mode: ThemeMode,
    theme: RenderTheme,
    blocks: Vec<Block>,
    pages: Vec<PlannedPage>,
    toc: Vec<TocEntry>,
    metadata: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct PlannedPage {
    items: Vec<PlannedItem>,
    text: String,
    links: Vec<LinkInfo>,
}

#[derive(Debug, Clone)]
struct PlannedItem {
    block: usize,
    units: Range<usize>,
    y: u32,
    height: u32,
}

#[derive(Debug, Clone)]
struct Block {
    kind: BlockKind,
    text: String,
    links: Vec<OwnedLink>,
    heading: Option<Heading>,
}

#[derive(Debug, Clone)]
enum BlockKind {
    Text(TextBlock),
    List(ListBlock),
    Table(TableBlock),
    Visual(VisualBlock),
    Rule,
    Blank,
}

#[derive(Debug, Clone)]
struct TextBlock {
    lines: Vec<Vec<InlineSpan>>,
    style: TextBlockStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextBlockStyle {
    Prose,
    Heading(u8),
    Code,
    Quote,
    Error,
}

#[derive(Debug, Clone)]
struct ListBlock {
    lines: Vec<Vec<InlineSpan>>,
}

#[derive(Debug, Clone)]
struct TableBlock {
    rows: Vec<Vec<Vec<InlineSpan>>>,
    alignments: Vec<TableAlignment>,
    widths: Vec<u32>,
    row_heights: Vec<u32>,
}

#[derive(Debug, Clone)]
struct VisualBlock {
    source: VisualSource,
    natural_width: u32,
    natural_height: u32,
    render_width: u32,
    render_height: u32,
    alt: String,
    semantic_text: String,
    links: Vec<OwnedLink>,
}

#[derive(Debug, Clone)]
enum VisualSource {
    RasterFile(PathBuf),
    SvgFile(PathBuf),
    DataUri(String),
    Mermaid(String),
    MermaidSvg(String),
    Placeholder { source: String, message: String },
}

#[derive(Debug, Clone)]
struct InlineSpan {
    span: TextSpan,
    href: Option<String>,
}

#[derive(Debug, Clone)]
struct OwnedLink {
    text: String,
    href: String,
}

#[derive(Debug, Clone)]
struct Heading {
    title: String,
    level: usize,
    slug: String,
}

#[derive(Debug, Clone)]
struct ImageSpec {
    url: String,
    alt: String,
    width_px: Option<u32>,
}

impl MarkupDocument {
    pub fn open(path: &Path, kind: MarkupKind, theme_mode: ThemeMode) -> Result<Self> {
        let source = read_bounded(path, MAX_SOURCE_BYTES)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let source = String::from_utf8(source).context("markup source is not valid UTF-8")?;
        Self::parse(
            &source,
            path.parent().unwrap_or_else(|| Path::new(".")),
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("document"),
            kind,
            theme_mode,
        )
    }

    pub fn parse(
        source: &str,
        base_dir: &Path,
        fallback_title: &str,
        kind: MarkupKind,
        theme_mode: ThemeMode,
    ) -> Result<Self> {
        ensure!(
            source.len() <= MAX_SOURCE_BYTES,
            "markup source exceeds the 16 MiB limit"
        );
        let theme = RenderTheme::for_mode(theme_mode);
        let (mut blocks, title) = match kind {
            MarkupKind::Markdown => parse_markdown(source, base_dir, fallback_title)?,
            MarkupKind::Mermaid => {
                let svg = render_mermaid_svg(source)
                    .context("standalone Mermaid source failed preflight")?;
                let (width, height) = svg_dimensions(&svg)?;
                let semantics = mermaid_semantics(&svg);
                let visual = visual_with_fit(
                    VisualSource::MermaidSvg(svg),
                    width,
                    height,
                    fallback_title.to_owned(),
                    semantics.0.clone(),
                    semantics.1,
                    None,
                );
                (
                    vec![Block {
                        text: semantics.0,
                        links: visual.links.clone(),
                        heading: None,
                        kind: BlockKind::Visual(visual),
                    }],
                    fallback_title.to_owned(),
                )
            }
        };
        ensure!(
            blocks.len() <= MAX_BLOCKS,
            "document exceeds the 100000-block limit"
        );
        let planned_units = blocks.iter().try_fold(0usize, |total, block| {
            total.checked_add(block_unit_count(block).max(1))
        });
        ensure!(
            planned_units.is_some_and(|count| count <= MAX_BLOCKS),
            "document exceeds the 100000-block limit"
        );
        if blocks.is_empty() {
            blocks.push(Block {
                kind: BlockKind::Blank,
                text: String::new(),
                links: Vec::new(),
                heading: None,
            });
        }

        let mut measurer = TextRenderer::new();
        let mut pages = paginate(&blocks, &theme, &mut measurer)?;
        if kind == MarkupKind::Mermaid
            && let Some(item) = pages.first_mut().and_then(|page| page.items.first_mut())
        {
            item.y = PAGE_HEIGHT.saturating_sub(item.height) / 2;
        }
        let mut slug_pages = HashMap::new();
        let mut toc = Vec::new();
        for (page_index, page) in pages.iter().enumerate() {
            for item in &page.items {
                if item.units.start != 0 {
                    continue;
                }
                if let Some(heading) = &blocks[item.block].heading
                    && slug_pages
                        .insert(heading.slug.clone(), page_index)
                        .is_none()
                {
                    toc.push(TocEntry {
                        title: heading.title.clone(),
                        page: page_index,
                        level: heading.level.saturating_sub(1),
                    });
                }
            }
        }
        populate_page_semantics(&mut pages, &blocks, &slug_pages, base_dir);
        let metadata = vec![
            ("Format".to_owned(), kind.format_name().to_owned()),
            ("Title".to_owned(), title.clone()),
            (
                "Theme".to_owned(),
                match theme_mode {
                    ThemeMode::Light => "Light",
                    ThemeMode::Dark => "Dark",
                }
                .to_owned(),
            ),
            ("Pages".to_owned(), pages.len().to_string()),
        ];
        Ok(Self {
            kind,
            theme_mode,
            theme,
            blocks,
            pages,
            toc,
            metadata,
        })
    }

    pub fn kind(&self) -> MarkupKind {
        self.kind
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn toc(&self) -> &[TocEntry] {
        &self.toc
    }

    pub fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    pub fn page_text(&self, page: usize) -> &str {
        self.pages
            .get(page)
            .map(|page| page.text.as_str())
            .unwrap_or("")
    }

    pub fn page_links(&self, page: usize) -> Vec<LinkInfo> {
        self.pages
            .get(page)
            .map(|page| page.links.clone())
            .unwrap_or_default()
    }

    pub fn search_counts(&self, term: &str) -> Vec<usize> {
        self.pages
            .iter()
            .map(|page| count_matches(&page.text, term))
            .collect()
    }

    pub fn render_page(&self, page: usize, search_term: Option<&str>) -> Result<MarkupPage> {
        let page = self
            .pages
            .get(page)
            .ok_or_else(|| anyhow!("page {} is out of range", page + 1))?;
        let mut canvas = Canvas::new(PAGE_WIDTH, PAGE_HEIGHT, self.theme.background);
        let mut text_renderer = TextRenderer::new();
        let mut highlights = Vec::new();
        for item in &page.items {
            let block = &self.blocks[item.block];
            let image = self.render_item(block, &item.units, &mut text_renderer)?;
            let x = PAGE_MARGIN + CONTENT_WIDTH.saturating_sub(image.width()) / 2;
            canvas.overlay(x as i32, item.y as i32, &image);
            if search_term.is_some_and(|term| contains_case_insensitive(&block.text, term)) {
                let occurrences =
                    count_matches(&block.text, search_term.unwrap_or_default()).max(1);
                highlights.extend((0..occurrences).map(|_| crate::compositor::HighlightRect {
                    x0: x,
                    y0: item.y,
                    x1: x.saturating_add(image.width()).min(PAGE_WIDTH),
                    y1: item.y.saturating_add(item.height).min(PAGE_HEIGHT),
                }));
            }
        }
        Ok(MarkupPage {
            image: canvas.into_image(),
            highlights,
        })
    }

    fn render_item(
        &self,
        block: &Block,
        units: &Range<usize>,
        text: &mut TextRenderer,
    ) -> Result<RgbaImage> {
        match &block.kind {
            BlockKind::Text(block) => {
                let spans = joined_lines(&block.lines[units.clone()]);
                Ok(render_text(text, &spans, block.style, &self.theme))
            }
            BlockKind::List(block) => {
                let spans = joined_lines(&block.lines[units.clone()]);
                Ok(render_text(
                    text,
                    &spans,
                    TextBlockStyle::Prose,
                    &self.theme,
                ))
            }
            BlockKind::Table(table) => render_table(text, table, units.clone(), &self.theme),
            BlockKind::Visual(visual) => self.render_visual(visual),
            BlockKind::Rule => {
                let mut canvas = Canvas::new(CONTENT_WIDTH, 32, self.theme.background);
                canvas.fill_rect(0, 15, CONTENT_WIDTH, 2, self.theme.border);
                Ok(canvas.into_image())
            }
            BlockKind::Blank => Ok(RgbaImage::from_pixel(
                CONTENT_WIDTH,
                1,
                self.theme.background,
            )),
        }
    }

    fn render_visual(&self, visual: &VisualBlock) -> Result<RgbaImage> {
        debug_assert!(visual.natural_width > 0 && visual.natural_height > 0);
        let max_width = visual.render_width.max(1);
        let max_height = visual.render_height.max(1);
        let result = match &visual.source {
            VisualSource::RasterFile(path) => {
                let bytes = read_bounded(path, MAX_ASSET_BYTES)
                    .with_context(|| format!("cannot read image {}", path.display()))?;
                decode_raster(&bytes).map(|image| fit_raster(&image, max_width, max_height))
            }
            VisualSource::SvgFile(path) => {
                let bytes = read_bounded(path, MAX_ASSET_BYTES)
                    .with_context(|| format!("cannot read SVG {}", path.display()))?;
                let svg = std::str::from_utf8(&bytes).context("SVG image is not UTF-8")?;
                rasterize_svg_to_fit(svg, self.theme.background, max_width, max_height)
            }
            VisualSource::DataUri(uri) => {
                render_data_uri(uri, self.theme.background, max_width, max_height)
            }
            VisualSource::Mermaid(source) => render_mermaid_svg(source).and_then(|svg| {
                rasterize_svg_to_fit(&svg, self.theme.background, max_width, max_height)
            }),
            VisualSource::MermaidSvg(svg) => {
                rasterize_svg_to_fit(svg, self.theme.background, max_width, max_height)
            }
            VisualSource::Placeholder { source, message } => Err(anyhow!("{message}: {source}")),
        };
        let image = match result {
            Ok(image) => image,
            Err(error) if matches!(visual.source, VisualSource::Mermaid(_)) => render_error_block(
                match &visual.source {
                    VisualSource::Mermaid(source) => source,
                    _ => "",
                },
                &error,
                CONTENT_WIDTH,
                self.theme_mode,
            ),
            Err(error) => render_placeholder(&visual.alt, &error.to_string(), &self.theme),
        };
        let image = fit_raster(&image, max_width, max_height);
        let mut canvas = Canvas::new(
            CONTENT_WIDTH,
            image.height().saturating_add(18),
            self.theme.background,
        );
        let x = CONTENT_WIDTH.saturating_sub(image.width()) / 2;
        canvas.overlay(x as i32, 9, &image);
        Ok(canvas.into_image())
    }
}

pub struct MarkupPage {
    pub image: RgbaImage,
    pub highlights: Vec<crate::compositor::HighlightRect>,
}

fn parse_markdown(
    input: &str,
    base_dir: &Path,
    fallback_title: &str,
) -> Result<(Vec<Block>, String)> {
    let arena = Arena::new();
    let rewritten = preprocess_markdown_image_width_syntax(input);
    let root = parse_document(&arena, &rewritten, &comrak_options());
    let mut parser = MarkdownParser {
        base_dir,
        blocks: Vec::new(),
        slug_counts: HashMap::new(),
        first_heading: None,
    };
    for child in root.children() {
        parser.parse_block(child)?;
    }
    Ok((
        parser.blocks,
        parser
            .first_heading
            .unwrap_or_else(|| fallback_title.to_owned()),
    ))
}

fn comrak_options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    options.extension.tagfilter = true;
    options
}

struct MarkdownParser<'a> {
    base_dir: &'a Path,
    blocks: Vec<Block>,
    slug_counts: HashMap<String, usize>,
    first_heading: Option<String>,
}

impl MarkdownParser<'_> {
    fn parse_block<'a>(&mut self, node: &'a AstNode<'a>) -> Result<()> {
        ensure!(
            self.blocks.len() < MAX_BLOCKS,
            "document exceeds the 100000-block limit"
        );
        match &node.data.borrow().value {
            NodeValue::Heading(heading) => {
                let inline = collect_inline(node);
                let title = inline_plain_text(&inline).trim().to_owned();
                self.first_heading.get_or_insert_with(|| title.clone());
                let slug = duplicate_safe_slug(&title, &mut self.slug_counts);
                let mut styled = inline;
                for span in &mut styled {
                    span.span.style.bold = true;
                }
                let lines = wrap_inline(styled.clone(), heading_char_budget(heading.level));
                self.blocks.push(Block {
                    text: title.clone(),
                    links: inline_links(&styled),
                    heading: Some(Heading {
                        title,
                        level: heading.level as usize,
                        slug,
                    }),
                    kind: BlockKind::Text(TextBlock {
                        lines,
                        style: TextBlockStyle::Heading(heading.level),
                    }),
                });
            }
            NodeValue::Paragraph => self.parse_paragraph(node)?,
            NodeValue::CodeBlock(code) => {
                if code.info.trim().eq_ignore_ascii_case("mermaid") {
                    let visual = embedded_mermaid(&code.literal);
                    self.blocks.push(Block {
                        text: visual.semantic_text.clone(),
                        links: visual.links.clone(),
                        heading: None,
                        kind: BlockKind::Visual(visual),
                    });
                } else {
                    let style = TextStyle {
                        code: true,
                        ..TextStyle::default()
                    };
                    let inline = vec![InlineSpan {
                        span: TextSpan {
                            text: code.literal.clone(),
                            style,
                        },
                        href: None,
                    }];
                    self.blocks.push(Block {
                        text: code.literal.clone(),
                        links: Vec::new(),
                        heading: None,
                        kind: BlockKind::Text(TextBlock {
                            lines: wrap_inline(inline, code_char_budget()),
                            style: TextBlockStyle::Code,
                        }),
                    });
                }
            }
            NodeValue::ThematicBreak => self.blocks.push(Block {
                kind: BlockKind::Rule,
                text: String::new(),
                links: Vec::new(),
                heading: None,
            }),
            NodeValue::BlockQuote => {
                let inline = collect_inline(node);
                self.blocks.push(Block {
                    text: inline_plain_text(&inline),
                    links: inline_links(&inline),
                    heading: None,
                    kind: BlockKind::Text(TextBlock {
                        lines: wrap_inline(inline, prose_char_budget().saturating_sub(4)),
                        style: TextBlockStyle::Quote,
                    }),
                });
            }
            NodeValue::List(list) => self.parse_list(node, *list),
            NodeValue::Table(table) => self.parse_table(node, &table.alignments),
            NodeValue::HtmlBlock(html) => {
                if is_html_comment(&html.literal) {
                    return Ok(());
                }
                if let Some(spec) = parse_html_image(&html.literal) {
                    self.push_visual(spec)?;
                } else {
                    let inline = vec![InlineSpan {
                        span: TextSpan {
                            text: html.literal.clone(),
                            style: TextStyle {
                                code: true,
                                ..TextStyle::default()
                            },
                        },
                        href: None,
                    }];
                    self.blocks.push(Block {
                        text: html.literal.clone(),
                        links: Vec::new(),
                        heading: None,
                        kind: BlockKind::Text(TextBlock {
                            lines: wrap_inline(inline, code_char_budget()),
                            style: TextBlockStyle::Code,
                        }),
                    });
                }
            }
            _ => {
                for child in node.children() {
                    self.parse_block(child)?;
                }
            }
        }
        Ok(())
    }

    fn parse_paragraph<'a>(&mut self, node: &'a AstNode<'a>) -> Result<()> {
        let units = collect_paragraph_units(node);
        let mut pending = Vec::new();
        for unit in units {
            match unit {
                ParagraphUnit::Inline(span) => pending.push(span),
                ParagraphUnit::Image(spec) => {
                    self.push_pending_text(&mut pending);
                    self.push_visual(spec)?;
                }
            }
        }
        self.push_pending_text(&mut pending);
        Ok(())
    }

    fn push_pending_text(&mut self, pending: &mut Vec<InlineSpan>) {
        if pending.is_empty() {
            return;
        }
        let inline = std::mem::take(pending);
        self.blocks.push(Block {
            text: inline_plain_text(&inline),
            links: inline_links(&inline),
            heading: None,
            kind: BlockKind::Text(TextBlock {
                lines: wrap_inline(inline, prose_char_budget()),
                style: TextBlockStyle::Prose,
            }),
        });
    }

    fn push_visual(&mut self, spec: ImageSpec) -> Result<()> {
        let visual = image_visual(spec, self.base_dir)?;
        self.blocks.push(Block {
            text: visual.semantic_text.clone(),
            links: visual.links.clone(),
            heading: None,
            kind: BlockKind::Visual(visual),
        });
        Ok(())
    }

    fn parse_list<'a>(&mut self, node: &'a AstNode<'a>, list: comrak::nodes::NodeList) {
        let mut ordinal = list.start.max(1);
        let mut lines = Vec::new();
        let mut text = String::new();
        let mut links = Vec::new();
        for item in node.children() {
            let marker = match list.list_type {
                ListType::Bullet => "• ".to_owned(),
                ListType::Ordered => {
                    let marker = format!("{ordinal}. ");
                    ordinal += 1;
                    marker
                }
            };
            let mut inline = vec![InlineSpan {
                span: TextSpan {
                    text: marker,
                    style: TextStyle {
                        bold: true,
                        ..TextStyle::default()
                    },
                },
                href: None,
            }];
            inline.extend(collect_inline(item));
            links.extend(inline_links(&inline));
            let plain = inline_plain_text(&inline);
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&plain);
            lines.extend(wrap_inline(inline, prose_char_budget().saturating_sub(4)));
        }
        self.blocks.push(Block {
            text,
            links,
            heading: None,
            kind: BlockKind::List(ListBlock { lines }),
        });
    }

    fn parse_table<'a>(&mut self, node: &'a AstNode<'a>, alignments: &[TableAlignment]) {
        let rows = table_rows(node);
        if rows.is_empty() {
            return;
        }
        let columns = rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(1)
            .max(alignments.len())
            .max(1);
        let widths = measured_column_widths(&rows, columns, CONTENT_WIDTH - columns as u32 - 1);
        let row_heights = rows
            .iter()
            .map(|row| table_row_height(row, &widths))
            .collect();
        let text = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| inline_plain_text(cell))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let links = rows
            .iter()
            .flat_map(|row| row.iter())
            .flat_map(|cell| inline_links(cell))
            .collect();
        self.blocks.push(Block {
            text,
            links,
            heading: None,
            kind: BlockKind::Table(TableBlock {
                rows,
                alignments: alignments.to_vec(),
                widths,
                row_heights,
            }),
        });
    }
}

#[derive(Debug)]
enum ParagraphUnit {
    Inline(InlineSpan),
    Image(ImageSpec),
}

fn collect_paragraph_units<'a>(node: &'a AstNode<'a>) -> Vec<ParagraphUnit> {
    let mut units = Vec::new();
    collect_inline_nodes(node, &TextStyle::default(), None, &mut units);
    units
}

fn collect_inline<'a>(node: &'a AstNode<'a>) -> Vec<InlineSpan> {
    collect_paragraph_units(node)
        .into_iter()
        .map(|unit| match unit {
            ParagraphUnit::Inline(span) => span,
            ParagraphUnit::Image(image) => InlineSpan {
                span: TextSpan {
                    text: if image.alt.is_empty() {
                        "[image]".to_owned()
                    } else {
                        format!("[image: {}]", image.alt)
                    },
                    style: TextStyle {
                        italic: true,
                        ..TextStyle::default()
                    },
                },
                href: None,
            },
        })
        .collect()
}

fn collect_inline_nodes<'a>(
    parent: &'a AstNode<'a>,
    style: &TextStyle,
    href: Option<&str>,
    units: &mut Vec<ParagraphUnit>,
) {
    let mut current_style = style.clone();
    for child in parent.children() {
        if let NodeValue::HtmlInline(tag) = &child.data.borrow().value {
            if tag.eq_ignore_ascii_case("<small>") {
                current_style.scale *= 0.78;
                continue;
            }
            if tag.eq_ignore_ascii_case("</small>") {
                current_style.scale /= 0.78;
                continue;
            }
            if tag.eq_ignore_ascii_case("<big>") {
                current_style.scale *= 1.25;
                continue;
            }
            if tag.eq_ignore_ascii_case("</big>") {
                current_style.scale /= 1.25;
                continue;
            }
        }
        let unit = match &child.data.borrow().value {
            NodeValue::Text(text) => Some(ParagraphUnit::Inline(InlineSpan {
                span: TextSpan {
                    text: text.clone().into_owned(),
                    style: current_style.clone(),
                },
                href: href.map(str::to_owned),
            })),
            NodeValue::Code(code) => {
                let mut next = current_style.clone();
                next.code = true;
                Some(ParagraphUnit::Inline(InlineSpan {
                    span: TextSpan {
                        text: code.literal.clone(),
                        style: next,
                    },
                    href: href.map(str::to_owned),
                }))
            }
            NodeValue::SoftBreak => Some(ParagraphUnit::Inline(InlineSpan {
                span: TextSpan {
                    text: " ".to_owned(),
                    style: current_style.clone(),
                },
                href: href.map(str::to_owned),
            })),
            NodeValue::LineBreak => Some(ParagraphUnit::Inline(InlineSpan {
                span: TextSpan {
                    text: "\n".to_owned(),
                    style: current_style.clone(),
                },
                href: href.map(str::to_owned),
            })),
            NodeValue::Strong => {
                let mut next = current_style.clone();
                next.bold = true;
                collect_inline_nodes(child, &next, href, units);
                None
            }
            NodeValue::Emph => {
                let mut next = current_style.clone();
                next.italic = true;
                collect_inline_nodes(child, &next, href, units);
                None
            }
            NodeValue::Strikethrough => {
                let mut next = current_style.clone();
                next.strike = true;
                collect_inline_nodes(child, &next, href, units);
                None
            }
            NodeValue::Link(link) => {
                let mut next = current_style.clone();
                next.link = true;
                collect_inline_nodes(child, &next, Some(&link.url), units);
                None
            }
            NodeValue::Image(link) => {
                let mut spec = parse_markdown_image_spec(&link.url);
                spec.alt = plain_text(child);
                Some(ParagraphUnit::Image(spec))
            }
            NodeValue::HtmlInline(tag) => {
                if is_html_comment(tag) {
                    None
                } else {
                    parse_html_image(tag).map(ParagraphUnit::Image)
                }
            }
            NodeValue::TaskItem(done) => Some(ParagraphUnit::Inline(InlineSpan {
                span: TextSpan {
                    text: if done.symbol.is_some() {
                        "☑ "
                    } else {
                        "☐ "
                    }
                    .to_owned(),
                    style: current_style.clone(),
                },
                href: href.map(str::to_owned),
            })),
            _ => {
                collect_inline_nodes(child, &current_style, href, units);
                None
            }
        };
        if let Some(unit) = unit {
            units.push(unit);
        }
    }
}

fn plain_text<'a>(node: &'a AstNode<'a>) -> String {
    let mut output = String::new();
    plain_text_into(node, &mut output);
    output
}

fn plain_text_into<'a>(node: &'a AstNode<'a>, output: &mut String) {
    match &node.data.borrow().value {
        NodeValue::Text(text) => output.push_str(text),
        NodeValue::Code(code) => output.push_str(&code.literal),
        NodeValue::SoftBreak | NodeValue::LineBreak => output.push(' '),
        _ => {
            for child in node.children() {
                plain_text_into(child, output);
            }
        }
    }
}

fn wrap_inline(spans: Vec<InlineSpan>, budget: usize) -> Vec<Vec<InlineSpan>> {
    let budget = budget.max(8);
    let mut lines = vec![Vec::new()];
    let mut used = 0usize;
    for inline in spans {
        let mut segment = String::new();
        for character in inline.span.text.chars() {
            if character == '\n' {
                push_inline_segment(&mut lines, &inline, &mut segment);
                lines.push(Vec::new());
                used = 0;
                continue;
            }
            if character.is_whitespace() && used >= budget {
                push_inline_segment(&mut lines, &inline, &mut segment);
                lines.push(Vec::new());
                used = 0;
                continue;
            }
            segment.push(character);
            used += 1;
            if used >= budget && !character.is_alphanumeric() {
                push_inline_segment(&mut lines, &inline, &mut segment);
                lines.push(Vec::new());
                used = 0;
            }
        }
        push_inline_segment(&mut lines, &inline, &mut segment);
    }
    lines.retain(|line| !line.is_empty());
    if lines.is_empty() {
        vec![vec![InlineSpan {
            span: TextSpan::plain(" "),
            href: None,
        }]]
    } else {
        lines
    }
}

fn push_inline_segment(lines: &mut [Vec<InlineSpan>], inline: &InlineSpan, segment: &mut String) {
    if segment.is_empty() {
        return;
    }
    lines.last_mut().unwrap().push(InlineSpan {
        span: TextSpan {
            text: std::mem::take(segment),
            style: inline.span.style.clone(),
        },
        href: inline.href.clone(),
    });
}

fn joined_lines(lines: &[Vec<InlineSpan>]) -> Vec<TextSpan> {
    let mut spans = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            spans.push(TextSpan::plain("\n"));
        }
        spans.extend(line.iter().map(|inline| inline.span.clone()));
    }
    if spans.is_empty() {
        spans.push(TextSpan::plain(" "));
    }
    spans
}

fn inline_plain_text(inline: &[InlineSpan]) -> String {
    inline.iter().map(|span| span.span.text.as_str()).collect()
}

fn inline_links(inline: &[InlineSpan]) -> Vec<OwnedLink> {
    let mut links: Vec<OwnedLink> = Vec::new();
    let mut last_href: Option<String> = None;
    for span in inline {
        let Some(href) = &span.href else {
            last_href = None;
            continue;
        };
        if last_href.as_ref() == Some(href) {
            if let Some(link) = links.last_mut() {
                link.text.push_str(&span.span.text);
            }
        } else {
            links.push(OwnedLink {
                text: span.span.text.clone(),
                href: href.clone(),
            });
            last_href = Some(href.clone());
        }
    }
    links
}

fn paginate(
    blocks: &[Block],
    theme: &RenderTheme,
    text: &mut TextRenderer,
) -> Result<Vec<PlannedPage>> {
    let mut pages = vec![PlannedPage {
        items: Vec::new(),
        text: String::new(),
        links: Vec::new(),
    }];
    let mut y = PAGE_MARGIN;
    for (block_index, block) in blocks.iter().enumerate() {
        let units = block_unit_count(block).max(1);
        let full_height = block_height(block, 0..units, theme, text);
        let remaining = PAGE_HEIGHT.saturating_sub(PAGE_MARGIN).saturating_sub(y);
        let keep_with_next = block.heading.is_some()
            && blocks.get(block_index + 1).is_some_and(|next| {
                let next_units = block_unit_count(next).max(1);
                full_height
                    .saturating_add(BLOCK_GAP)
                    .saturating_add(block_height(next, 0..next_units, theme, text))
                    <= CONTENT_HEIGHT
            });
        if keep_with_next
            && blocks.get(block_index + 1).is_some_and(|next| {
                let next_units = block_unit_count(next).max(1);
                full_height
                    .saturating_add(BLOCK_GAP)
                    .saturating_add(block_height(next, 0..next_units, theme, text))
                    > remaining
            })
            && !pages.last().unwrap().items.is_empty()
        {
            push_page(&mut pages)?;
            y = PAGE_MARGIN;
        }

        if full_height <= CONTENT_HEIGHT {
            if full_height > PAGE_HEIGHT.saturating_sub(PAGE_MARGIN).saturating_sub(y)
                && !pages.last().unwrap().items.is_empty()
            {
                push_page(&mut pages)?;
                y = PAGE_MARGIN;
            }
            place_item(&mut pages, block_index, 0..units, y, full_height);
            y = y.saturating_add(full_height).saturating_add(BLOCK_GAP);
            continue;
        }

        let mut start = 0usize;
        while start < units {
            if PAGE_HEIGHT.saturating_sub(PAGE_MARGIN).saturating_sub(y)
                < minimum_unit_height(block, theme, text)
                && !pages.last().unwrap().items.is_empty()
            {
                push_page(&mut pages)?;
                y = PAGE_MARGIN;
            }
            let available = PAGE_HEIGHT.saturating_sub(PAGE_MARGIN).saturating_sub(y);
            let mut end = start + 1;
            while end <= units && block_height(block, start..end, theme, text) <= available {
                end += 1;
            }
            let end = end.saturating_sub(1).max(start + 1).min(units);
            let height = block_height(block, start..end, theme, text).min(CONTENT_HEIGHT);
            place_item(&mut pages, block_index, start..end, y, height);
            start = end;
            y = y.saturating_add(height).saturating_add(BLOCK_GAP);
            if start < units {
                push_page(&mut pages)?;
                y = PAGE_MARGIN;
            }
        }
    }
    if pages.len() > 1 && pages.last().is_some_and(|page| page.items.is_empty()) {
        pages.pop();
    }
    Ok(pages)
}

fn push_page(pages: &mut Vec<PlannedPage>) -> Result<()> {
    ensure!(
        pages.len() < MAX_PAGES,
        "document exceeds the 10000-page limit"
    );
    pages.push(PlannedPage {
        items: Vec::new(),
        text: String::new(),
        links: Vec::new(),
    });
    Ok(())
}

fn place_item(pages: &mut [PlannedPage], block: usize, units: Range<usize>, y: u32, height: u32) {
    pages.last_mut().unwrap().items.push(PlannedItem {
        block,
        units,
        y,
        height,
    });
}

fn block_unit_count(block: &Block) -> usize {
    match &block.kind {
        BlockKind::Text(block) => block.lines.len(),
        BlockKind::List(block) => block.lines.len(),
        BlockKind::Table(block) => block.rows.len().saturating_sub(1).max(1),
        BlockKind::Visual(_) | BlockKind::Rule | BlockKind::Blank => 1,
    }
}

fn block_height(
    block: &Block,
    units: Range<usize>,
    theme: &RenderTheme,
    text: &mut TextRenderer,
) -> u32 {
    match &block.kind {
        BlockKind::Text(block) => {
            measure_text(text, &joined_lines(&block.lines[units]), block.style, theme)
        }
        BlockKind::List(block) => measure_text(
            text,
            &joined_lines(&block.lines[units]),
            TextBlockStyle::Prose,
            theme,
        ),
        BlockKind::Table(table) => table_fragment_height(table, units),
        BlockKind::Visual(visual) => visual.render_height.saturating_add(18),
        BlockKind::Rule => 32,
        BlockKind::Blank => 1,
    }
}

fn minimum_unit_height(block: &Block, theme: &RenderTheme, text: &mut TextRenderer) -> u32 {
    block_height(block, 0..1, theme, text).min(CONTENT_HEIGHT)
}

fn text_options(style: TextBlockStyle, theme: &RenderTheme) -> TextBlockOptions {
    let (font_size, line_height, padding_x, padding_y, background, default_color) = match style {
        TextBlockStyle::Prose => (
            PROSE_FONT_SIZE,
            PROSE_LINE_HEIGHT,
            0,
            12,
            theme.background,
            theme.text,
        ),
        TextBlockStyle::Heading(level) => {
            let size = match level {
                1 => 57.0,
                2 => 48.0,
                3 => 40.0,
                4 => 35.0,
                _ => 31.0,
            };
            (size, size * 1.28, 0, 12, theme.background, theme.text)
        }
        TextBlockStyle::Code => (
            CODE_FONT_SIZE,
            CODE_LINE_HEIGHT,
            24,
            18,
            theme.code_bg,
            theme.code_text,
        ),
        TextBlockStyle::Quote => (29.0, 42.0, 28, 16, theme.blockquote_bg, theme.muted_text),
        TextBlockStyle::Error => (27.0, 38.0, 24, 18, theme.error_bg, theme.error_text),
    };
    TextBlockOptions {
        width: CONTENT_WIDTH,
        padding_x,
        padding_y,
        font_size,
        line_height,
        background,
        default_color,
        link_color: theme.link,
        code_color: theme.code_text,
        code_background: theme.code_bg,
    }
}

fn measure_text(
    renderer: &mut TextRenderer,
    spans: &[TextSpan],
    style: TextBlockStyle,
    theme: &RenderTheme,
) -> u32 {
    renderer.measure_text_block(spans, &text_options(style, theme))
}

fn render_text(
    renderer: &mut TextRenderer,
    spans: &[TextSpan],
    style: TextBlockStyle,
    theme: &RenderTheme,
) -> RgbaImage {
    let image = renderer.render_text_block(spans, &text_options(style, theme));
    if style == TextBlockStyle::Quote {
        let mut canvas = Canvas::new(CONTENT_WIDTH, image.height(), theme.background);
        canvas.fill_rect(0, 0, 5, image.height(), theme.blockquote_bar);
        canvas.overlay(10, 0, &image);
        canvas.into_image()
    } else {
        image
    }
}

fn table_rows<'a>(node: &'a AstNode<'a>) -> Vec<Vec<Vec<InlineSpan>>> {
    node.children()
        .filter(|row| matches!(row.data.borrow().value, NodeValue::TableRow(_)))
        .map(|row| row.children().map(collect_inline).collect())
        .collect()
}

fn measured_column_widths(
    rows: &[Vec<Vec<InlineSpan>>],
    columns: usize,
    available: u32,
) -> Vec<u32> {
    let equal = (available / columns as u32).max(1);
    let mut weights = vec![1usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(columns) {
            weights[index] = weights[index].max(inline_plain_text(cell).chars().count().min(80));
        }
    }
    let total: usize = weights.iter().sum();
    if total == 0 {
        return vec![equal; columns];
    }
    let minimum = 120u32.min(equal);
    let mut widths: Vec<u32> = weights
        .iter()
        .map(|weight| {
            ((available as usize * *weight / total) as u32)
                .max(minimum)
                .min(equal.saturating_mul(2))
        })
        .collect();
    let sum: u32 = widths.iter().sum();
    if sum > available {
        widths.fill(equal);
    }
    widths
}

fn table_row_height(row: &[Vec<InlineSpan>], widths: &[u32]) -> u32 {
    row.iter()
        .enumerate()
        .map(|(column, cell)| {
            let width = widths
                .get(column)
                .copied()
                .unwrap_or(120)
                .saturating_sub(16);
            let chars = inline_plain_text(cell).chars().count().max(1) as u32;
            let lines = chars.div_ceil((width / 16).max(1));
            lines.saturating_mul(34).saturating_add(16)
        })
        .max()
        .unwrap_or(50)
}

fn table_fragment_height(table: &TableBlock, body_units: Range<usize>) -> u32 {
    let header = table.row_heights.first().copied().unwrap_or(1);
    let body_start = if table.rows.len() > 1 {
        body_units.start + 1
    } else {
        0
    };
    let body_end = if table.rows.len() > 1 {
        (body_units.end + 1).min(table.rows.len())
    } else {
        1
    };
    let body = table.row_heights[body_start..body_end]
        .iter()
        .fold(0u32, |sum, height| sum.saturating_add(*height + 1));
    header
        .saturating_add(body)
        .saturating_add(3)
        .min(CONTENT_HEIGHT)
}

fn render_table(
    renderer: &mut TextRenderer,
    table: &TableBlock,
    body_units: Range<usize>,
    theme: &RenderTheme,
) -> Result<RgbaImage> {
    let mut row_indices = vec![0usize];
    if table.rows.len() > 1 {
        row_indices.extend((body_units.start + 1)..(body_units.end + 1).min(table.rows.len()));
    }
    let height = table_fragment_height(table, body_units);
    let table_width = table.widths.iter().sum::<u32>() + table.widths.len() as u32 + 1;
    let mut canvas = Canvas::new(CONTENT_WIDTH, height, theme.background);
    canvas.stroke_rect(0, 0, table_width, height, theme.border);
    let mut y = 1i32;
    for (visible_row, row_index) in row_indices.into_iter().enumerate() {
        let row = &table.rows[row_index];
        let row_height = table.row_heights[row_index];
        let mut x = 1i32;
        for (column, width) in table.widths.iter().copied().enumerate() {
            let _alignment = table.alignments.get(column);
            let background = if visible_row == 0 {
                theme.table_header_bg
            } else {
                theme.background
            };
            canvas.fill_rect(x, y, width, row_height, background);
            let cell = row.get(column).cloned().unwrap_or_default();
            let mut opts = text_options(TextBlockStyle::Prose, theme);
            opts.width = width;
            opts.padding_x = 8;
            opts.padding_y = 7;
            opts.font_size = 25.0;
            opts.line_height = 34.0;
            opts.background = background;
            let image = renderer.render_text_block(&joined_lines(&[cell]), &opts);
            canvas.overlay(x, y, &image);
            x += width as i32;
            canvas.fill_rect(x, y - 1, 1, row_height + 2, theme.border);
            x += 1;
        }
        y += row_height as i32;
        canvas.fill_rect(0, y, table_width, 1, theme.border);
        y += 1;
    }
    Ok(canvas.into_image())
}

fn image_visual(spec: ImageSpec, base_dir: &Path) -> Result<VisualBlock> {
    let lower = spec.url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(placeholder_visual(
            spec.alt,
            spec.url,
            "remote images are not supported".to_owned(),
        ));
    }
    if lower.starts_with("data:image/") {
        return Ok(match data_uri_dimensions(&spec.url) {
            Ok((width, height)) => visual_with_fit(
                VisualSource::DataUri(spec.url),
                width,
                height,
                spec.alt.clone(),
                spec.alt,
                Vec::new(),
                spec.width_px,
            ),
            Err(error) => placeholder_visual(spec.alt, spec.url, error.to_string()),
        });
    }
    let path_part = spec.url.split(['#', '?']).next().unwrap_or(&spec.url);
    let path = Path::new(path_part);
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        base_dir.join(path)
    };
    let bytes = match read_bounded(&path, MAX_ASSET_BYTES) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(placeholder_visual(spec.alt, spec.url, error.to_string()));
        }
    };
    let svg = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"));
    let dimensions = if svg {
        std::str::from_utf8(&bytes)
            .context("SVG image is not UTF-8")
            .and_then(svg_dimensions)
    } else {
        encoded_raster_dimensions(&bytes)
    };
    match dimensions {
        Ok((width, height)) => Ok(visual_with_fit(
            if svg {
                VisualSource::SvgFile(path)
            } else {
                VisualSource::RasterFile(path)
            },
            width,
            height,
            spec.alt.clone(),
            spec.alt,
            Vec::new(),
            spec.width_px,
        )),
        Err(error) => Ok(placeholder_visual(spec.alt, spec.url, error.to_string())),
    }
}

fn embedded_mermaid(source: &str) -> VisualBlock {
    match render_mermaid_svg(source) {
        Ok(svg) => match svg_dimensions(&svg) {
            Ok((width, height)) => {
                let (text, links) = mermaid_semantics(&svg);
                visual_with_fit(
                    VisualSource::Mermaid(source.to_owned()),
                    width,
                    height,
                    "Mermaid diagram".to_owned(),
                    text,
                    links,
                    None,
                )
            }
            Err(error) => placeholder_visual(
                "Mermaid diagram".to_owned(),
                source.to_owned(),
                error.to_string(),
            ),
        },
        Err(error) => {
            let height = (source.lines().count() as u32)
                .saturating_add(3)
                .saturating_mul(CODE_LINE_HEIGHT as u32)
                .saturating_add(36)
                .min(CONTENT_HEIGHT);
            VisualBlock {
                source: VisualSource::Mermaid(source.to_owned()),
                natural_width: CONTENT_WIDTH,
                natural_height: height,
                render_width: CONTENT_WIDTH,
                render_height: height,
                alt: "Mermaid diagram".to_owned(),
                semantic_text: format!("Mermaid render failed: {error}"),
                links: Vec::new(),
            }
        }
    }
}

fn placeholder_visual(alt: String, source: String, message: String) -> VisualBlock {
    VisualBlock {
        source: VisualSource::Placeholder { source, message },
        natural_width: CONTENT_WIDTH,
        natural_height: 180,
        render_width: CONTENT_WIDTH,
        render_height: 180,
        semantic_text: alt.clone(),
        alt,
        links: Vec::new(),
    }
}

fn visual_with_fit(
    source: VisualSource,
    width: u32,
    height: u32,
    alt: String,
    semantic_text: String,
    links: Vec<OwnedLink>,
    requested_width: Option<u32>,
) -> VisualBlock {
    let width_limit = requested_width
        .unwrap_or(CONTENT_WIDTH)
        .clamp(1, CONTENT_WIDTH);
    let scale = (width_limit as f64 / width as f64)
        .min(CONTENT_HEIGHT as f64 / height as f64)
        .min(1.0);
    let render_width = ((width as f64 * scale).round() as u32).max(1);
    let render_height = ((height as f64 * scale).round() as u32).max(1);
    VisualBlock {
        source,
        natural_width: width,
        natural_height: height,
        render_width,
        render_height,
        alt,
        semantic_text,
        links,
    }
}

fn render_placeholder(alt: &str, message: &str, theme: &RenderTheme) -> RgbaImage {
    let spans = vec![
        TextSpan {
            text: if alt.is_empty() {
                "Image unavailable\n".to_owned()
            } else {
                format!("Image unavailable: {alt}\n")
            },
            style: TextStyle {
                bold: true,
                ..TextStyle::default()
            },
        },
        TextSpan {
            text: message.to_owned(),
            style: TextStyle {
                code: true,
                ..TextStyle::default()
            },
        },
    ];
    TextRenderer::new().render_text_block(&spans, &text_options(TextBlockStyle::Error, theme))
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = std::fs::metadata(path)?;
    ensure!(
        metadata.len() <= limit as u64,
        "{} exceeds the {} MiB encoded-size limit",
        path.display(),
        limit / (1024 * 1024)
    );
    let bytes = std::fs::read(path)?;
    ensure!(
        bytes.len() <= limit,
        "{} grew beyond the encoded-size limit while being read",
        path.display()
    );
    Ok(bytes)
}

fn encoded_raster_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    ensure!(
        bytes.len() <= MAX_ASSET_BYTES,
        "encoded image exceeds the 32 MiB limit"
    );
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("unknown raster image format")?;
    let (width, height) = reader
        .into_dimensions()
        .context("cannot read image dimensions")?;
    validate_raster_size(width, height)?;
    Ok((width, height))
}

fn decode_raster(bytes: &[u8]) -> Result<RgbaImage> {
    let (width, height) = encoded_raster_dimensions(bytes)?;
    let image = image::load_from_memory(bytes)
        .context("cannot decode raster image")?
        .to_rgba8();
    ensure!(
        image.dimensions() == (width, height),
        "decoded image dimensions changed during decode"
    );
    Ok(image)
}

fn fit_raster(image: &RgbaImage, max_width: u32, max_height: u32) -> RgbaImage {
    if image.width() <= max_width && image.height() <= max_height {
        return image.clone();
    }
    let scale =
        (max_width as f64 / image.width() as f64).min(max_height as f64 / image.height() as f64);
    let width = ((image.width() as f64 * scale).round() as u32).max(1);
    let height = ((image.height() as f64 * scale).round() as u32).max(1);
    image::imageops::resize(image, width, height, FilterType::Lanczos3)
}

fn data_uri_bytes(uri: &str) -> Result<(&str, Vec<u8>)> {
    let (metadata, encoded) = uri
        .split_once(',')
        .ok_or_else(|| anyhow!("invalid data URI image"))?;
    ensure!(
        metadata.to_ascii_lowercase().starts_with("data:image/"),
        "data URI is not an image"
    );
    ensure!(
        metadata.to_ascii_lowercase().contains(";base64"),
        "only base64 data URI images are supported"
    );
    ensure!(
        encoded.len() <= MAX_ASSET_BYTES,
        "encoded data URI exceeds the 32 MiB limit"
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("invalid base64 data URI image")?;
    ensure!(
        bytes.len() <= MAX_ASSET_BYTES,
        "decoded data URI exceeds the 32 MiB limit"
    );
    Ok((metadata, bytes))
}

fn data_uri_dimensions(uri: &str) -> Result<(u32, u32)> {
    let (metadata, bytes) = data_uri_bytes(uri)?;
    if metadata
        .to_ascii_lowercase()
        .starts_with("data:image/svg+xml")
    {
        svg_dimensions(std::str::from_utf8(&bytes).context("SVG data URI is not UTF-8")?)
    } else {
        encoded_raster_dimensions(&bytes)
    }
}

fn render_data_uri(
    uri: &str,
    background: Rgba<u8>,
    max_width: u32,
    max_height: u32,
) -> Result<RgbaImage> {
    let (metadata, bytes) = data_uri_bytes(uri)?;
    if metadata
        .to_ascii_lowercase()
        .starts_with("data:image/svg+xml")
    {
        rasterize_svg_to_fit(
            std::str::from_utf8(&bytes).context("SVG data URI is not UTF-8")?,
            background,
            max_width,
            max_height,
        )
    } else {
        decode_raster(&bytes).map(|image| fit_raster(&image, max_width, max_height))
    }
}

fn mermaid_semantics(svg: &str) -> (String, Vec<OwnedLink>) {
    let Ok(document) = roxmltree::Document::parse(svg) else {
        return (String::new(), Vec::new());
    };
    let labels: Vec<String> = document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "text")
        .filter_map(|node| {
            let text = node
                .descendants()
                .filter_map(|descendant| descendant.text())
                .collect::<Vec<_>>()
                .join(" ");
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            (!text.is_empty()).then_some(text)
        })
        .collect();
    let links = document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "a")
        .filter_map(|node| {
            let href = node.attribute("href").or_else(|| {
                node.attributes()
                    .find(|attribute| attribute.name() == "href")
                    .map(|attribute| attribute.value())
            })?;
            let text = node
                .descendants()
                .filter_map(|descendant| descendant.text())
                .collect::<Vec<_>>()
                .join(" ");
            Some(OwnedLink {
                text: text.trim().to_owned(),
                href: href.to_owned(),
            })
        })
        .collect();
    (labels.join("\n"), links)
}

fn populate_page_semantics(
    pages: &mut [PlannedPage],
    blocks: &[Block],
    slug_pages: &HashMap<String, usize>,
    base_dir: &Path,
) {
    for page in pages {
        let mut text = Vec::new();
        let mut links = Vec::new();
        let mut seen = HashSet::new();
        for item in &page.items {
            let block = &blocks[item.block];
            if !block.text.is_empty() {
                text.push(block.text.clone());
            }
            for link in block.links.iter().chain(match &block.kind {
                BlockKind::Visual(visual) => visual.links.iter(),
                _ => [].iter(),
            }) {
                let page_target = link
                    .href
                    .strip_prefix('#')
                    .and_then(|slug| slug_pages.get(slug))
                    .copied();
                let href = if page_target.is_some() {
                    link.href.clone()
                } else {
                    resolve_link_href(&link.href, base_dir)
                };
                let key = (href.clone(), page_target);
                if seen.insert(key) {
                    links.push(LinkInfo {
                        text: if link.text.trim().is_empty() {
                            href.clone()
                        } else {
                            link.text.clone()
                        },
                        uri: href,
                        page: page_target,
                    });
                }
            }
        }
        page.text = text.join("\n");
        page.links = links;
    }
}

fn resolve_link_href(href: &str, base_dir: &Path) -> String {
    let lower = href.to_ascii_lowercase();
    if href.starts_with('#')
        || href.starts_with('/')
        || lower.contains("://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
    {
        return href.to_owned();
    }
    base_dir.join(href).to_string_lossy().into_owned()
}

fn duplicate_safe_slug(title: &str, counts: &mut HashMap<String, usize>) -> String {
    let mut base = String::new();
    let mut pending_dash = false;
    for character in title.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() || character == '_' {
            if pending_dash && !base.is_empty() {
                base.push('-');
            }
            pending_dash = false;
            base.push(character);
        } else if character.is_whitespace() || character == '-' {
            pending_dash = true;
        }
    }
    if base.is_empty() {
        base.push_str("section");
    }
    let count = counts.entry(base.clone()).or_default();
    let slug = if *count == 0 {
        base
    } else {
        format!("{base}-{count}")
    };
    *count += 1;
    slug
}

fn count_matches(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack
        .to_lowercase()
        .match_indices(&needle.to_lowercase())
        .count()
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    !needle.is_empty() && haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn prose_char_budget() -> usize {
    (CONTENT_WIDTH as f32 / (PROSE_FONT_SIZE * 0.62)) as usize
}

fn code_char_budget() -> usize {
    (CONTENT_WIDTH as f32 / (CODE_FONT_SIZE * 0.62)) as usize
}

fn heading_char_budget(level: u8) -> usize {
    let size = match level {
        1 => 57.0,
        2 => 48.0,
        3 => 40.0,
        4 => 35.0,
        _ => 31.0,
    };
    (CONTENT_WIDTH as f32 / (size * 0.62)) as usize
}

fn parse_markdown_image_spec(raw: &str) -> ImageSpec {
    let raw = raw.trim();
    let Some((url, metadata)) = raw.rsplit_once('|') else {
        return ImageSpec {
            url: raw.to_owned(),
            alt: String::new(),
            width_px: None,
        };
    };
    let width_px = metadata.split_whitespace().find_map(|token| {
        let (name, value) = token.split_once('=')?;
        name.eq_ignore_ascii_case("width")
            .then(|| parse_image_width(value))
            .flatten()
    });
    if width_px.is_none() {
        return ImageSpec {
            url: raw.to_owned(),
            alt: String::new(),
            width_px: None,
        };
    }
    ImageSpec {
        url: url.trim().to_owned(),
        alt: String::new(),
        width_px,
    }
}

fn parse_image_width(value: &str) -> Option<u32> {
    value
        .trim()
        .trim_matches(['"', '\''])
        .trim_end_matches("px")
        .trim()
        .parse()
        .ok()
        .filter(|width| *width > 0)
}

fn is_html_comment(html: &str) -> bool {
    let html = html.trim();
    html.starts_with("<!--") && html.ends_with("-->")
}

fn parse_html_image(html: &str) -> Option<ImageSpec> {
    let trimmed = html.trim();
    let content = trimmed.strip_prefix('<')?.trim_start();
    if content.len() < 3 || !content[..3].eq_ignore_ascii_case("img") {
        return None;
    }
    let attributes = parse_html_attributes(
        content[3..]
            .trim()
            .trim_end_matches('>')
            .trim_end_matches('/')
            .trim(),
    );
    let url = attributes
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("src"))
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty())?;
    let alt = attributes
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("alt"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let width_px = attributes
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("width"))
        .and_then(|(_, value)| parse_image_width(value));
    Some(ImageSpec { url, alt, width_px })
}

fn parse_html_attributes(input: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let mut index = 0usize;
    let bytes = input.as_bytes();
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b'/') {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'/'
        {
            index += 1;
        }
        let name = input[start..index].to_ascii_lowercase();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let value = if index < bytes.len() && bytes[index] == b'=' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index < bytes.len() && matches!(bytes[index], b'"' | b'\'') {
                let quote = bytes[index];
                index += 1;
                let start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                let value = input[start..index].to_owned();
                index = (index + 1).min(bytes.len());
                value
            } else {
                let start = index;
                while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                input[start..index].trim().to_owned()
            }
        } else {
            String::new()
        };
        attributes.push((name, value));
    }
    attributes
}

fn preprocess_markdown_image_width_syntax(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut fence: Option<char> = None;
    for segment in input.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map(|line| (line, "\n"))
            .unwrap_or((segment, ""));
        let marker = line
            .trim_start()
            .starts_with("```")
            .then_some('`')
            .or_else(|| line.trim_start().starts_with("~~~").then_some('~'));
        if let Some(marker) = marker {
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
            output.push_str(line);
        } else if fence.is_some() {
            output.push_str(line);
        } else {
            output.push_str(&rewrite_markdown_image_width_line(line));
        }
        output.push_str(newline);
    }
    output
}

fn rewrite_markdown_image_width_line(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut cursor = 0usize;
    while let Some(relative_start) = line[cursor..].find("![") {
        let start = cursor + relative_start;
        output.push_str(&line[cursor..start]);
        let alt_start = start + 2;
        let Some(relative_alt_end) = line[alt_start..].find("](") else {
            output.push_str(&line[start..]);
            return output;
        };
        let destination_start = alt_start + relative_alt_end + 2;
        let Some(relative_end) = line[destination_start..].find(')') else {
            output.push_str(&line[start..]);
            return output;
        };
        let end = destination_start + relative_end;
        let destination = &line[destination_start..end];
        if destination.trim_start().starts_with('<')
            || parse_markdown_image_spec(destination).width_px.is_none()
        {
            output.push_str(&line[start..=end]);
        } else {
            output.push_str(&line[start..destination_start]);
            output.push('<');
            output.push_str(destination);
            output.push_str(">)");
        }
        cursor = end + 1;
    }
    output.push_str(&line[cursor..]);
    output
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "vvrd-markdown-test-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    fn png_bytes(color: Rgba<u8>) -> Vec<u8> {
        let image = RgbaImage::from_pixel(8, 6, color);
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn document(source: &str) -> MarkupDocument {
        MarkupDocument::parse(
            source,
            Path::new("."),
            "test",
            MarkupKind::Markdown,
            ThemeMode::Light,
        )
        .unwrap()
    }

    #[test]
    fn letter_geometry_and_empty_document_are_stable() {
        assert_eq!((PAGE_WIDTH, PAGE_HEIGHT, PAGE_MARGIN), (2040, 2640, 180));
        let document = document("");
        assert_eq!(document.page_count(), 1);
        assert_eq!(
            document.render_page(0, None).unwrap().image.dimensions(),
            (PAGE_WIDTH, PAGE_HEIGHT)
        );
    }

    #[test]
    fn duplicate_headings_get_github_style_slugs_and_toc_pages() {
        let document = document("# Hello, World!\n\n[a](#hello-world)\n\n# Hello, World!");
        assert_eq!(document.toc.len(), 2);
        assert_eq!(document.page_links(0)[0].page, Some(0));
        assert_eq!(
            duplicate_safe_slug("Hello, World!", &mut HashMap::new()),
            "hello-world"
        );
    }

    #[test]
    fn relative_links_are_resolved_from_the_document_directory() {
        let directory = Path::new("/tmp/vvrd-guide");
        let document = MarkupDocument::parse(
            "[appendix](parts/appendix.md)",
            directory,
            "guide",
            MarkupKind::Markdown,
            ThemeMode::Light,
        )
        .unwrap();
        assert_eq!(
            document.page_links(0)[0].uri,
            directory
                .join("parts/appendix.md")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn heading_stays_with_following_block() {
        let prefix = "paragraph\n\n".repeat(35);
        let document = document(&format!("{prefix}\n# Kept\n\nfollowing paragraph"));
        let heading = document
            .toc
            .iter()
            .find(|entry| entry.title == "Kept")
            .unwrap();
        assert!(
            document.pages[heading.page]
                .text
                .contains("following paragraph")
        );
    }

    #[test]
    fn oversized_code_splits_without_exceeding_letter_pages() {
        let source = format!("```\n{}\n```", "line of code\n".repeat(500));
        let document = document(&source);
        assert!(document.page_count() > 1);
        assert!(document.page_count() < MAX_PAGES);
    }

    #[test]
    fn gfm_features_are_owned_and_searchable() {
        let document = document(
            "- [x] task\n\n~~strike~~ <https://example.com>\n\n| A | B |\n|---|---|\n| c | d |",
        );
        assert_eq!(document.search_counts("task").iter().sum::<usize>(), 1);
        assert!(
            document
                .page_links(0)
                .iter()
                .any(|link| link.uri == "https://example.com")
        );
        assert!(document.page_text(0).contains("A\tB"));
    }

    #[test]
    fn remote_image_is_an_in_page_placeholder() {
        let document = document("![remote](https://example.com/image.png)");
        assert_eq!(document.page_count(), 1);
        assert_eq!(
            document.render_page(0, None).unwrap().image.dimensions(),
            (PAGE_WIDTH, PAGE_HEIGHT)
        );
    }

    #[test]
    fn invalid_embedded_mermaid_is_an_error_block() {
        let document = document("```mermaid\nthis is not a diagram\n```");
        assert_eq!(document.page_count(), 1);
        assert!(document.page_text(0).contains("Mermaid"));
    }

    #[test]
    fn invalid_standalone_mermaid_fails_preflight() {
        assert!(
            MarkupDocument::parse(
                "not mermaid",
                Path::new("."),
                "bad",
                MarkupKind::Mermaid,
                ThemeMode::Light
            )
            .is_err()
        );
    }

    #[test]
    fn both_themes_render_different_paper_colors() {
        let light = document("hello").render_page(0, None).unwrap();
        let dark = MarkupDocument::parse(
            "hello",
            Path::new("."),
            "test",
            MarkupKind::Markdown,
            ThemeMode::Dark,
        )
        .unwrap()
        .render_page(0, None)
        .unwrap();
        assert_ne!(light.image.get_pixel(0, 0), dark.image.get_pixel(0, 0));
    }

    #[test]
    fn search_produces_page_scoped_counts_and_geometry() {
        let document = document("needle needle");
        assert_eq!(document.search_counts("needle"), vec![2]);
        assert_eq!(
            document
                .render_page(0, Some("needle"))
                .unwrap()
                .highlights
                .len(),
            2
        );
    }

    #[test]
    fn local_raster_svg_and_data_uri_assets_render_inside_the_content_box() {
        let directory = temp_dir();
        std::fs::write(
            directory.join("image.png"),
            png_bytes(Rgba([200, 20, 10, 255])),
        )
        .unwrap();
        std::fs::write(
            directory.join("image.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="30"><rect width="40" height="30" fill="blue"/></svg>"#,
        )
        .unwrap();
        let data =
            base64::engine::general_purpose::STANDARD.encode(png_bytes(Rgba([10, 200, 20, 255])));
        let source = format!(
            "![raster](image.png)\n\n![svg](image.svg)\n\n![data](data:image/png;base64,{data})"
        );
        let document = MarkupDocument::parse(
            &source,
            &directory,
            "assets",
            MarkupKind::Markdown,
            ThemeMode::Light,
        )
        .unwrap();
        let page = document.render_page(0, None).unwrap();
        assert_eq!(page.image.dimensions(), (PAGE_WIDTH, PAGE_HEIGHT));
        assert!(
            document
                .blocks
                .iter()
                .filter_map(|block| {
                    let BlockKind::Visual(visual) = &block.kind else {
                        return None;
                    };
                    Some((visual.render_width, visual.render_height))
                })
                .all(|(width, height)| width <= CONTENT_WIDTH && height <= CONTENT_HEIGHT)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn standalone_mermaid_exposes_label_and_link_semantics() {
        let document = MarkupDocument::parse(
            "flowchart LR\nA[Alpha]-->B[Beta]\nclick A \"https://example.com\"",
            Path::new("."),
            "diagram",
            MarkupKind::Mermaid,
            ThemeMode::Light,
        )
        .unwrap();
        assert!(document.page_text(0).contains("Alpha"));
        assert!(
            document
                .page_links(0)
                .iter()
                .any(|link| link.uri == "https://example.com")
        );
        let item = &document.pages[0].items[0];
        assert_eq!(item.y, PAGE_HEIGHT.saturating_sub(item.height) / 2);
    }

    #[test]
    fn allocation_and_page_count_limits_are_checked() {
        assert!(
            MarkupDocument::parse(
                &"x".repeat(MAX_SOURCE_BYTES + 1),
                Path::new("."),
                "large",
                MarkupKind::Markdown,
                ThemeMode::Light
            )
            .is_err()
        );
        assert!(validate_raster_size(16_384, 16_384).is_err());
        let mut pages = (0..MAX_PAGES)
            .map(|_| PlannedPage {
                items: Vec::new(),
                text: String::new(),
                links: Vec::new(),
            })
            .collect();
        assert!(push_page(&mut pages).is_err());
    }
}
