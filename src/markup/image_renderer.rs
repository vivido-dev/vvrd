#![allow(dead_code)]

use std::io::Cursor;

use anyhow::{Context, Result};
use cosmic_text::{
    Attrs, Buffer, Color as CosmicColor, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    Weight,
};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

use crate::markup::fonts;

pub const META_STRIKE: usize = 1;
pub const META_CODE: usize = 1 << 1;

#[derive(Debug, Clone)]
pub struct RenderedImage {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

impl RenderedImage {
    pub fn from_rgba(image: &RgbaImage) -> Result<Self> {
        Self::from_rgba_owned(image.clone())
    }

    pub fn from_rgba_owned(image: RgbaImage) -> Result<Self> {
        let width = image.width();
        let height = image.height();
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .context("failed to encode PNG")?;
        Ok(Self { width, height, png })
    }

    #[allow(dead_code)]
    pub fn from_dynamic(image: DynamicImage) -> Result<Self> {
        Self::from_rgba(&image.to_rgba8())
    }

    #[allow(dead_code)]
    pub fn to_rgba(&self) -> Result<RgbaImage> {
        Ok(image::load_from_memory(&self.png)
            .context("failed to decode rendered PNG")?
            .to_rgba8())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Debug, Clone)]
pub struct RenderTheme {
    pub background: Rgba<u8>,
    pub text: Rgba<u8>,
    pub muted_text: Rgba<u8>,
    pub link: Rgba<u8>,
    pub code_text: Rgba<u8>,
    pub code_bg: Rgba<u8>,
    pub border: Rgba<u8>,
    pub table_header_bg: Rgba<u8>,
    pub blockquote_bg: Rgba<u8>,
    pub blockquote_bar: Rgba<u8>,
    pub error_bg: Rgba<u8>,
    pub error_text: Rgba<u8>,
}

impl RenderTheme {
    pub fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self {
                background: rgba(20, 24, 28, 255),
                text: rgba(230, 235, 241, 255),
                muted_text: rgba(165, 174, 185, 255),
                link: rgba(86, 156, 214, 255),
                code_text: rgba(236, 220, 181, 255),
                code_bg: rgba(36, 42, 49, 255),
                border: rgba(83, 94, 108, 255),
                table_header_bg: rgba(33, 39, 47, 255),
                blockquote_bg: rgba(26, 31, 37, 255),
                blockquote_bar: rgba(104, 118, 135, 255),
                error_bg: rgba(68, 31, 36, 255),
                error_text: rgba(255, 198, 204, 255),
            },
            ThemeMode::Light => Self {
                background: rgba(250, 250, 248, 255),
                text: rgba(28, 31, 35, 255),
                muted_text: rgba(94, 102, 112, 255),
                link: rgba(9, 105, 218, 255),
                code_text: rgba(96, 44, 0, 255),
                code_bg: rgba(238, 240, 242, 255),
                border: rgba(190, 197, 207, 255),
                table_header_bg: rgba(238, 241, 245, 255),
                blockquote_bg: rgba(242, 244, 247, 255),
                blockquote_bar: rgba(146, 154, 166, 255),
                error_bg: rgba(255, 231, 235, 255),
                error_text: rgba(130, 25, 38, 255),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: bool,
    pub strike: bool,
    pub scale: f32,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            bold: false,
            italic: false,
            code: false,
            link: false,
            strike: false,
            scale: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextSpan {
    pub text: String,
    pub style: TextStyle,
}

impl TextSpan {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: TextStyle::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextBlockOptions {
    pub width: u32,
    pub padding_x: u32,
    pub padding_y: u32,
    pub font_size: f32,
    pub line_height: f32,
    pub background: Rgba<u8>,
    pub default_color: Rgba<u8>,
    pub link_color: Rgba<u8>,
    pub code_color: Rgba<u8>,
    pub code_background: Rgba<u8>,
}

pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl TextRenderer {
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new_with_fonts(fonts::bundled_font_sources()),
            swash_cache: SwashCache::new(),
        }
    }

    pub fn render_text_block(&mut self, spans: &[TextSpan], opts: &TextBlockOptions) -> RgbaImage {
        let (mut buffer, height) = self.layout_text_block(spans, opts);
        let mut image = RgbaImage::from_pixel(opts.width.max(1), height, opts.background);
        self.draw_metadata_backgrounds(&mut image, &buffer, opts);
        self.draw_buffer(&mut image, &mut buffer, opts);
        self.draw_strikethroughs(&mut image, &buffer, opts);
        image
    }

    /// Measure a rich-text block through the same shaping path used for rasterization.
    ///
    /// Pagination calls this without allocating a page-sized bitmap. That keeps the page plan
    /// lightweight while guaranteeing that the requested page uses the same font metrics.
    pub fn measure_text_block(&mut self, spans: &[TextSpan], opts: &TextBlockOptions) -> u32 {
        self.layout_text_block(spans, opts).1
    }

    fn layout_text_block(&mut self, spans: &[TextSpan], opts: &TextBlockOptions) -> (Buffer, u32) {
        let content_width = opts
            .width
            .saturating_sub(opts.padding_x.saturating_mul(2))
            .max(1);
        let metrics = Metrics::new(opts.font_size, opts.line_height);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(Some(content_width as f32), None);

        let default_attrs = self.attrs_for_style(&TextStyle::default(), opts);
        let pieces: Vec<(&str, Attrs<'_>)> = spans
            .iter()
            .filter(|span| !span.text.is_empty())
            .map(|span| (span.text.as_str(), self.attrs_for_style(&span.style, opts)))
            .collect();

        if pieces.is_empty() {
            buffer.set_text("", &default_attrs, Shaping::Advanced, None);
        } else {
            buffer.set_rich_text(
                pieces.iter().map(|(text, attrs)| (*text, attrs.clone())),
                &default_attrs,
                Shaping::Advanced,
                None,
            );
        }

        buffer.shape_until_scroll(&mut self.font_system, false);
        let text_height = buffer
            .layout_runs()
            .map(|run| run.line_top + run.line_height)
            .fold(opts.line_height, f32::max)
            .ceil()
            .max(opts.line_height) as u32;
        let height = text_height
            .saturating_add(opts.padding_y.saturating_mul(2))
            .max(1);
        (buffer, height)
    }

    fn attrs_for_style<'a>(&self, style: &TextStyle, opts: &TextBlockOptions) -> Attrs<'a> {
        let color = if style.code {
            opts.code_color
        } else if style.link {
            opts.link_color
        } else {
            opts.default_color
        };
        let size = (opts.font_size * style.scale.max(0.25)).max(4.0);
        let line_height = (opts.line_height * style.scale.max(0.25)).max(size);
        let mut attrs = Attrs::new()
            .color(to_cosmic(color))
            .family(if style.code {
                Family::Name(fonts::CODE_FONT_FAMILY)
            } else {
                Family::Name(fonts::PROSE_FONT_FAMILY)
            })
            .weight(if style.bold {
                Weight::BOLD
            } else {
                Weight::NORMAL
            })
            .style(if style.italic {
                Style::Italic
            } else {
                Style::Normal
            })
            .metrics(Metrics::new(size, line_height));
        let mut metadata = 0;
        if style.strike {
            metadata |= META_STRIKE;
        }
        if style.code {
            metadata |= META_CODE;
        }
        attrs = attrs.metadata(metadata);
        attrs
    }

    fn draw_metadata_backgrounds(
        &self,
        image: &mut RgbaImage,
        buffer: &Buffer,
        opts: &TextBlockOptions,
    ) {
        for run in buffer.layout_runs() {
            let mut start: Option<f32> = None;
            let mut end: f32 = 0.0;
            for glyph in run.glyphs {
                if glyph.metadata & META_CODE != 0 {
                    start.get_or_insert(glyph.x);
                    end = end.max(glyph.x + glyph.w);
                } else if let Some(x0) = start.take() {
                    fill_rect(
                        image,
                        opts.padding_x as i32 + x0.floor() as i32 - 3,
                        opts.padding_y as i32 + run.line_top.floor() as i32 + 2,
                        (end - x0).ceil() as u32 + 6,
                        run.line_height.max(1.0).ceil() as u32 - 3,
                        opts.code_background,
                    );
                }
            }
            if let Some(x0) = start {
                fill_rect(
                    image,
                    opts.padding_x as i32 + x0.floor() as i32 - 3,
                    opts.padding_y as i32 + run.line_top.floor() as i32 + 2,
                    (end - x0).ceil() as u32 + 6,
                    run.line_height.max(1.0).ceil() as u32 - 3,
                    opts.code_background,
                );
            }
        }
    }

    fn draw_buffer(&mut self, image: &mut RgbaImage, buffer: &mut Buffer, opts: &TextBlockOptions) {
        let offset_x = opts.padding_x as i32;
        let offset_y = opts.padding_y as i32;
        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            to_cosmic(opts.default_color),
            |x, y, w, h, color| {
                fill_rect(
                    image,
                    offset_x + x,
                    offset_y + y,
                    w,
                    h,
                    Rgba(color.as_rgba()),
                );
            },
        );
    }

    fn draw_strikethroughs(&self, image: &mut RgbaImage, buffer: &Buffer, opts: &TextBlockOptions) {
        for run in buffer.layout_runs() {
            let y = opts.padding_y as i32
                + (run.line_top + run.line_height * 0.55).round().max(0.0) as i32;
            let mut start: Option<f32> = None;
            let mut end: f32 = 0.0;
            for glyph in run.glyphs {
                if glyph.metadata & META_STRIKE != 0 {
                    start.get_or_insert(glyph.x);
                    end = end.max(glyph.x + glyph.w);
                } else if let Some(x0) = start.take() {
                    fill_rect(
                        image,
                        opts.padding_x as i32 + x0.floor() as i32,
                        y,
                        (end - x0).ceil() as u32,
                        2,
                        opts.default_color,
                    );
                }
            }
            if let Some(x0) = start {
                fill_rect(
                    image,
                    opts.padding_x as i32 + x0.floor() as i32,
                    y,
                    (end - x0).ceil() as u32,
                    2,
                    opts.default_color,
                );
            }
        }
    }
}

impl Default for TextRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct Canvas {
    image: RgbaImage,
}

impl Canvas {
    pub fn new(width: u32, height: u32, background: Rgba<u8>) -> Self {
        Self {
            image: RgbaImage::from_pixel(width.max(1), height.max(1), background),
        }
    }

    #[allow(dead_code)]
    pub fn width(&self) -> u32 {
        self.image.width()
    }

    #[allow(dead_code)]
    pub fn height(&self) -> u32 {
        self.image.height()
    }

    pub fn into_image(self) -> RgbaImage {
        self.image
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, width: u32, height: u32, color: Rgba<u8>) {
        fill_rect(&mut self.image, x, y, width, height, color);
    }

    pub fn stroke_rect(&mut self, x: i32, y: i32, width: u32, height: u32, color: Rgba<u8>) {
        if width == 0 || height == 0 {
            return;
        }
        self.fill_rect(x, y, width, 1, color);
        self.fill_rect(x, y + height as i32 - 1, width, 1, color);
        self.fill_rect(x, y, 1, height, color);
        self.fill_rect(x + width as i32 - 1, y, 1, height, color);
    }

    pub fn overlay(&mut self, x: i32, y: i32, src: &RgbaImage) {
        overlay(&mut self.image, x, y, src);
    }
}

pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Rgba<u8> {
    Rgba([r, g, b, a])
}

pub fn fill_rect(image: &mut RgbaImage, x: i32, y: i32, width: u32, height: u32, color: Rgba<u8>) {
    if width == 0 || height == 0 {
        return;
    }
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = (x.saturating_add(width as i32)).max(0) as u32;
    let y1 = (y.saturating_add(height as i32)).max(0) as u32;
    let x1 = x1.min(image.width());
    let y1 = y1.min(image.height());
    for yy in y0..y1 {
        for xx in x0..x1 {
            blend_pixel(image, xx, yy, color);
        }
    }
}

pub fn overlay(dst: &mut RgbaImage, x: i32, y: i32, src: &RgbaImage) {
    for sy in 0..src.height() {
        for sx in 0..src.width() {
            let dx = x + sx as i32;
            let dy = y + sy as i32;
            if dx >= 0 && dy >= 0 && dx < dst.width() as i32 && dy < dst.height() as i32 {
                blend_pixel(dst, dx as u32, dy as u32, *src.get_pixel(sx, sy));
            }
        }
    }
}

pub fn resize_to_width(image: &RgbaImage, max_width: u32) -> RgbaImage {
    if image.width() <= max_width || max_width == 0 {
        return image.clone();
    }
    let height = ((image.height() as f32) * (max_width as f32 / image.width() as f32))
        .round()
        .max(1.0) as u32;
    image::imageops::resize(
        image,
        max_width,
        height,
        image::imageops::FilterType::Lanczos3,
    )
}

pub fn resize_to_exact_width(image: &RgbaImage, width: u32) -> RgbaImage {
    if width == 0 || image.width() == 0 || image.width() == width {
        return image.clone();
    }
    let height = ((image.height() as f32) * (width as f32 / image.width() as f32))
        .round()
        .max(1.0) as u32;
    image::imageops::resize(image, width, height, image::imageops::FilterType::Lanczos3)
}

fn blend_pixel(image: &mut RgbaImage, x: u32, y: u32, src: Rgba<u8>) {
    let alpha = src[3] as f32 / 255.0;
    if alpha <= 0.0 {
        return;
    }
    if alpha >= 1.0 {
        image.put_pixel(x, y, src);
        return;
    }
    let mut dst = *image.get_pixel(x, y);
    for idx in 0..3 {
        dst[idx] = ((src[idx] as f32 * alpha) + (dst[idx] as f32 * (1.0 - alpha))).round() as u8;
    }
    dst[3] = 255;
    image.put_pixel(x, y, dst);
}

fn to_cosmic(color: Rgba<u8>) -> CosmicColor {
    CosmicColor::rgba(color[0], color[1], color[2], color[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_image_round_trips_png() {
        let img = RgbaImage::from_pixel(16, 12, rgba(10, 20, 30, 255));
        let rendered = RenderedImage::from_rgba(&img).unwrap();
        assert_eq!(rendered.width, 16);
        assert_eq!(rendered.height, 12);
        assert!(!rendered.png.is_empty());
        assert_eq!(rendered.to_rgba().unwrap().dimensions(), (16, 12));
    }

    #[test]
    fn text_renderer_outputs_non_empty_pixels() {
        let mut renderer = TextRenderer::new();
        let theme = RenderTheme::for_mode(ThemeMode::Dark);
        let image = renderer.render_text_block(
            &[TextSpan::plain("hello")],
            &TextBlockOptions {
                width: 200,
                padding_x: 8,
                padding_y: 8,
                font_size: 16.0,
                line_height: 22.0,
                background: theme.background,
                default_color: theme.text,
                link_color: theme.link,
                code_color: theme.code_text,
                code_background: theme.code_bg,
            },
        );
        assert!(image.pixels().any(|pixel| *pixel != theme.background));
    }
}
