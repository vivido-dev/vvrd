use anyhow::{Context as _, ensure};
use image::{ImageBuffer, Rgb, RgbImage, imageops::FilterType};

use crate::geometry::WindowSize;

#[derive(Debug, Clone)]
pub struct PageImage {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub row_stride: usize,
    pub highlights: Vec<HighlightRect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ViewTransform {
    pub offset_x: u32,
    pub offset_y: u32,
    pub auto_crop: bool,
}

pub struct ComposedFrame {
    pub rgba: Vec<u8>,
    pub content_width: u32,
    pub content_height: u32,
}

impl PageImage {
    pub fn into_rgb(self) -> anyhow::Result<RgbImage> {
        let tight_stride = usize::try_from(self.width)
            .context("page width exceeds address space")?
            .checked_mul(3)
            .context("page row size overflow")?;
        ensure!(self.row_stride >= tight_stride, "invalid page row stride");
        let required = self
            .row_stride
            .checked_mul(self.height as usize)
            .context("page buffer size overflow")?;
        ensure!(
            self.pixels.len() >= required,
            "page pixel buffer is truncated"
        );

        let pixels = if self.row_stride == tight_stride {
            self.pixels
        } else {
            let mut packed = Vec::with_capacity(tight_stride * self.height as usize);
            for row in self
                .pixels
                .chunks(self.row_stride)
                .take(self.height as usize)
            {
                packed.extend_from_slice(&row[..tight_stride]);
            }
            packed
        };
        ImageBuffer::<Rgb<u8>, _>::from_raw(self.width, self.height, pixels)
            .context("cannot construct page image")
    }
}

pub fn compose_view(
    page: PageImage,
    viewport: WindowSize,
    transform: ViewTransform,
) -> anyhow::Result<ComposedFrame> {
    let width = viewport.page_area_width_px();
    let height = viewport.page_area_height_px();
    ensure!(width > 0 && height > 0, "viewport has no drawable pixels");
    let highlights = page.highlights.clone();
    let mut image = page.into_rgb()?;
    ensure!(
        image.width() > 0 && image.height() > 0,
        "page image is empty"
    );
    apply_highlights(&mut image, &highlights);

    if transform.auto_crop {
        image = crop_whitespace_image(image);
    }

    let (content_width, content_height, image) =
        if image.width() <= width && image.height() <= height && !transform.auto_crop {
            (image.width(), image.height(), image)
        } else if image.width() <= width && image.height() <= height {
            let scale =
                (width as f64 / image.width() as f64).min(height as f64 / image.height() as f64);
            let scaled_width = ((image.width() as f64 * scale).round() as u32).clamp(1, width);
            let scaled_height = ((image.height() as f64 * scale).round() as u32).clamp(1, height);
            (
                scaled_width,
                scaled_height,
                image::imageops::resize(&image, scaled_width, scaled_height, FilterType::Lanczos3),
            )
        } else {
            (image.width(), image.height(), image)
        };

    let mut rgba = vec![0_u8; viewport.framebuffer_len()?];
    let source_x = transform.offset_x.min(content_width.saturating_sub(width));
    let source_y = transform
        .offset_y
        .min(content_height.saturating_sub(height));
    let draw_width = content_width.saturating_sub(source_x).min(width);
    let draw_height = content_height.saturating_sub(source_y).min(height);
    let destination_x = width.saturating_sub(draw_width) / 2;
    let destination_y = height.saturating_sub(draw_height) / 2;
    for y in 0..draw_height {
        for x in 0..draw_width {
            let pixel = image.get_pixel(x + source_x, y + source_y);
            let index = (((y + destination_y) as usize * width as usize)
                + (x + destination_x) as usize)
                * 4;
            rgba[index..index + 3].copy_from_slice(&pixel.0);
            rgba[index + 3] = u8::MAX;
        }
    }
    Ok(ComposedFrame {
        rgba,
        content_width,
        content_height,
    })
}

fn apply_highlights(image: &mut RgbImage, highlights: &[HighlightRect]) {
    for rect in highlights {
        let x0 = rect.x0.min(image.width());
        let x1 = rect.x1.min(image.width());
        let y0 = rect.y0.min(image.height());
        let y1 = rect.y1.min(image.height());
        for y in y0..y1 {
            for x in x0..x1 {
                let pixel = image.get_pixel_mut(x, y);
                pixel.0[0] = pixel.0[0].saturating_add(80);
                pixel.0[1] = pixel.0[1].saturating_add(80);
                pixel.0[2] /= 2;
            }
        }
    }
}

fn whitespace_bounds(image: &RgbImage) -> Option<(u32, u32, u32, u32)> {
    const THRESHOLD: u8 = 240;
    const MARGIN: u32 = 8;
    let mut min_x = image.width();
    let mut min_y = image.height();
    let mut max_x = 0;
    let mut max_y = 0;
    let mut found = false;
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel.0.iter().any(|channel| *channel < THRESHOLD) {
            found = true;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    found.then(|| {
        let x0 = min_x.saturating_sub(MARGIN);
        let y0 = min_y.saturating_sub(MARGIN);
        let x1 = max_x
            .saturating_add(MARGIN)
            .saturating_add(1)
            .min(image.width());
        let y1 = max_y
            .saturating_add(MARGIN)
            .saturating_add(1)
            .min(image.height());
        (x0, y0, x1 - x0, y1 - y0)
    })
}

pub fn crop_whitespace_image(image: RgbImage) -> RgbImage {
    if let Some((x, y, width, height)) = whitespace_bounds(&image) {
        image::imageops::crop_imm(&image, x, y, width, height).to_image()
    } else {
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(pixels: Vec<u8>, width: u32, height: u32, row_stride: usize) -> PageImage {
        PageImage {
            pixels,
            width,
            height,
            row_stride,
            highlights: Vec::new(),
        }
    }

    #[test]
    fn centers_a_page_without_changing_viewport_size() {
        let page = page(vec![255, 0, 0, 255, 0, 0], 2, 1, 6);
        let viewport = WindowSize::from_cells(1, 2, 4, 4);
        let frame = compose_view(page, viewport, ViewTransform::default())
            .unwrap()
            .rgba;
        assert_eq!(frame.len(), 4 * 4 * 4);
        assert_eq!(&frame[0..4], &[0, 0, 0, 0]);
        assert!(frame.chunks_exact(4).any(|pixel| pixel == [255, 0, 0, 255]));
    }

    #[test]
    fn accepts_padded_rgb_rows() {
        let page = page(vec![1, 2, 3, 99, 4, 5, 6, 99], 1, 2, 4);
        assert!(
            compose_view(
                page,
                WindowSize::from_cells(1, 2, 1, 2),
                ViewTransform::default(),
            )
            .is_ok()
        );
    }

    #[test]
    fn view_offsets_crop_a_large_page() {
        let page = page((0..8).flat_map(|value| [value, 0, 0]).collect(), 8, 1, 24);
        let frame = compose_view(
            page,
            WindowSize::from_cells(1, 2, 4, 1),
            ViewTransform {
                offset_x: 3,
                offset_y: 0,
                auto_crop: false,
            },
        )
        .unwrap();
        assert_eq!(frame.content_width, 8);
        assert_eq!(frame.rgba[0], 3);
    }
}
