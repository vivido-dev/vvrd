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
    /// Scale the whole fixed page into the viewport. Zoom mode clears this and crops instead.
    pub fit_to_viewport: bool,
}

pub struct ComposedFrame {
    pub rgba: Vec<u8>,
    pub content_width: u32,
    pub content_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaOperation {
    Copy {
        destination_x: u32,
        destination_y: u32,
        width: u32,
        height: u32,
        source_x: u32,
        source_y: u32,
    },
    Overwrite {
        rect: DamageRect,
        rgba: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameDelta {
    pub operations: Vec<DeltaOperation>,
    pub damaged_pixels: u64,
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
        if transform.fit_to_viewport && (image.width() > width || image.height() > height) {
            let scale =
                (width as f64 / image.width() as f64).min(height as f64 / image.height() as f64);
            let scaled_width = ((image.width() as f64 * scale).round() as u32).clamp(1, width);
            let scaled_height = ((image.height() as f64 * scale).round() as u32).clamp(1, height);
            (
                scaled_width,
                scaled_height,
                image::imageops::resize(&image, scaled_width, scaled_height, FilterType::Lanczos3),
            )
        } else if image.width() <= width && image.height() <= height && !transform.auto_crop {
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

pub fn plan_frame_delta(
    previous: &ComposedFrame,
    current: &ComposedFrame,
    viewport: WindowSize,
    previous_transform: ViewTransform,
    current_transform: ViewTransform,
    same_page_image: bool,
    operation_limit: u32,
) -> anyhow::Result<Option<FrameDelta>> {
    let width = viewport.page_area_width_px();
    let height = viewport.page_area_height_px();
    ensure!(
        previous.rgba.len() == viewport.framebuffer_len()?
            && current.rgba.len() == previous.rgba.len(),
        "delta frame dimensions do not match the viewport"
    );
    if previous.rgba == current.rgba {
        return Ok(None);
    }

    if same_page_image
        && previous_transform.auto_crop == current_transform.auto_crop
        && let Some(delta) = plan_translation_delta(
            &previous.rgba,
            &current.rgba,
            width,
            height,
            previous_transform,
            current_transform,
            operation_limit,
        )?
    {
        return Ok(Some(delta));
    }

    let Some(rect) = changed_bounds(&previous.rgba, &current.rgba, width, height) else {
        return Ok(None);
    };
    if operation_limit == 0 {
        return Ok(None);
    }
    Ok(Some(FrameDelta {
        damaged_pixels: u64::from(rect.width) * u64::from(rect.height),
        operations: vec![DeltaOperation::Overwrite {
            rgba: extract_rect(&current.rgba, width, rect)?,
            rect,
        }],
    }))
}

fn plan_translation_delta(
    previous: &[u8],
    current: &[u8],
    width: u32,
    height: u32,
    previous_transform: ViewTransform,
    current_transform: ViewTransform,
    operation_limit: u32,
) -> anyhow::Result<Option<FrameDelta>> {
    let shift_x = i64::from(previous_transform.offset_x) - i64::from(current_transform.offset_x);
    let shift_y = i64::from(previous_transform.offset_y) - i64::from(current_transform.offset_y);
    if shift_x == 0 && shift_y == 0 {
        return Ok(None);
    }
    let (source_x, destination_x, copy_width) = translation_axis(width, shift_x);
    let (source_y, destination_y, copy_height) = translation_axis(height, shift_y);
    if copy_width == 0 || copy_height == 0 {
        return Ok(None);
    }

    let mut rects = Vec::with_capacity(4);
    push_rect(&mut rects, 0, 0, width, destination_y);
    push_rect(
        &mut rects,
        0,
        destination_y.saturating_add(copy_height),
        width,
        height.saturating_sub(destination_y.saturating_add(copy_height)),
    );
    push_rect(&mut rects, 0, destination_y, destination_x, copy_height);
    push_rect(
        &mut rects,
        destination_x.saturating_add(copy_width),
        destination_y,
        width.saturating_sub(destination_x.saturating_add(copy_width)),
        copy_height,
    );
    if 1_usize.saturating_add(rects.len()) > operation_limit as usize {
        return Ok(None);
    }

    let mut operations = Vec::with_capacity(1 + rects.len());
    operations.push(DeltaOperation::Copy {
        destination_x,
        destination_y,
        width: copy_width,
        height: copy_height,
        source_x,
        source_y,
    });
    for rect in rects {
        operations.push(DeltaOperation::Overwrite {
            rgba: extract_rect(current, width, rect)?,
            rect,
        });
    }
    let damage =
        u64::from(width) * u64::from(height) - u64::from(copy_width) * u64::from(copy_height);
    let delta = FrameDelta {
        operations,
        damaged_pixels: damage,
    };
    if apply_delta_reference(previous, width, height, &delta)? == current {
        Ok(Some(delta))
    } else {
        Ok(None)
    }
}

fn translation_axis(length: u32, shift: i64) -> (u32, u32, u32) {
    let magnitude = u32::try_from(shift.unsigned_abs())
        .unwrap_or(u32::MAX)
        .min(length);
    if shift >= 0 {
        (0, magnitude, length - magnitude)
    } else {
        (magnitude, 0, length - magnitude)
    }
}

fn push_rect(rects: &mut Vec<DamageRect>, x: u32, y: u32, width: u32, height: u32) {
    if width != 0 && height != 0 {
        rects.push(DamageRect {
            x,
            y,
            width,
            height,
        });
    }
}

fn changed_bounds(previous: &[u8], current: &[u8], width: u32, height: u32) -> Option<DamageRect> {
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut changed = false;
    for (index, (before, after)) in previous
        .chunks_exact(4)
        .zip(current.chunks_exact(4))
        .enumerate()
    {
        if before != after {
            let index = u32::try_from(index).ok()?;
            let x = index % width;
            let y = index / width;
            changed = true;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    changed.then_some(DamageRect {
        x: min_x,
        y: min_y,
        width: max_x - min_x + 1,
        height: max_y - min_y + 1,
    })
}

fn extract_rect(frame: &[u8], frame_width: u32, rect: DamageRect) -> anyhow::Result<Vec<u8>> {
    let row_bytes = usize::try_from(rect.width)
        .context("damage width exceeds address space")?
        .checked_mul(4)
        .context("damage row size overflow")?;
    let mut rgba = Vec::with_capacity(
        row_bytes
            .checked_mul(rect.height as usize)
            .context("damage buffer size overflow")?,
    );
    for y in rect.y..rect.y + rect.height {
        let start = (usize::try_from(y)
            .context("damage y exceeds address space")?
            .checked_mul(frame_width as usize)
            .and_then(|value| value.checked_add(rect.x as usize))
            .and_then(|value| value.checked_mul(4)))
        .context("damage offset overflow")?;
        rgba.extend_from_slice(&frame[start..start + row_bytes]);
    }
    Ok(rgba)
}

fn apply_delta_reference(
    previous: &[u8],
    frame_width: u32,
    frame_height: u32,
    delta: &FrameDelta,
) -> anyhow::Result<Vec<u8>> {
    let mut frame = previous.to_vec();
    for operation in &delta.operations {
        match operation {
            DeltaOperation::Copy {
                destination_x,
                destination_y,
                width,
                height,
                source_x,
                source_y,
            } => {
                let source = extract_rect(
                    &frame,
                    frame_width,
                    DamageRect {
                        x: *source_x,
                        y: *source_y,
                        width: *width,
                        height: *height,
                    },
                )?;
                overwrite_rect(
                    &mut frame,
                    frame_width,
                    DamageRect {
                        x: *destination_x,
                        y: *destination_y,
                        width: *width,
                        height: *height,
                    },
                    &source,
                )?;
            }
            DeltaOperation::Overwrite { rect, rgba } => {
                overwrite_rect(&mut frame, frame_width, *rect, rgba)?;
            }
        }
    }
    ensure!(
        frame.len()
            == usize::try_from(frame_width)?
                .checked_mul(frame_height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .context("reference frame size overflow")?,
        "reference frame size mismatch"
    );
    Ok(frame)
}

fn overwrite_rect(
    frame: &mut [u8],
    frame_width: u32,
    rect: DamageRect,
    rgba: &[u8],
) -> anyhow::Result<()> {
    let row_bytes = rect.width as usize * 4;
    ensure!(
        rgba.len() == row_bytes * rect.height as usize,
        "damage payload size mismatch"
    );
    for row in 0..rect.height as usize {
        let destination = ((rect.y as usize + row) * frame_width as usize + rect.x as usize) * 4;
        let source = row * row_bytes;
        frame[destination..destination + row_bytes]
            .copy_from_slice(&rgba[source..source + row_bytes]);
    }
    Ok(())
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
                fit_to_viewport: false,
            },
        )
        .unwrap();
        assert_eq!(frame.content_width, 8);
        assert_eq!(frame.rgba[0], 3);
    }

    #[test]
    fn fixed_page_can_be_contain_fitted_for_normal_view() {
        let page = page(vec![255; 8 * 8 * 3], 8, 8, 24);
        let frame = compose_view(
            page,
            WindowSize::from_cells(1, 2, 4, 4),
            ViewTransform {
                fit_to_viewport: true,
                ..ViewTransform::default()
            },
        )
        .unwrap();
        assert_eq!((frame.content_width, frame.content_height), (4, 4));
        assert!(frame.rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    fn composed_rgba(width: u32, height: u32, values: &[u8]) -> ComposedFrame {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for value in values {
            rgba.extend_from_slice(&[*value, 0, 0, 255]);
        }
        ComposedFrame {
            rgba,
            content_width: width,
            content_height: height,
        }
    }

    #[test]
    fn scroll_delta_uses_overlap_safe_copy_and_exposed_strip() {
        let viewport = WindowSize::from_cells(4, 5, 1, 1);
        let previous = composed_rgba(4, 4, &(0..16).collect::<Vec<_>>());
        let current = composed_rgba(
            4,
            4,
            &[4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 90, 91, 92, 93],
        );
        let delta = plan_frame_delta(
            &previous,
            &current,
            viewport,
            ViewTransform::default(),
            ViewTransform {
                offset_y: 1,
                ..ViewTransform::default()
            },
            true,
            16,
        )
        .unwrap()
        .unwrap();
        assert_eq!(delta.operations.len(), 2);
        assert_eq!(delta.damaged_pixels, 4);
        assert!(matches!(
            delta.operations[0],
            DeltaOperation::Copy {
                destination_y: 0,
                source_y: 1,
                height: 3,
                ..
            }
        ));
        assert_eq!(
            apply_delta_reference(&previous.rgba, 4, 4, &delta).unwrap(),
            current.rgba
        );
    }

    #[test]
    fn local_change_becomes_one_tight_overwrite() {
        let viewport = WindowSize::from_cells(6, 5, 1, 1);
        let previous = composed_rgba(6, 4, &[0; 24]);
        let mut values = [0; 24];
        values[8] = 1;
        values[9] = 2;
        values[14] = 3;
        values[15] = 4;
        let current = composed_rgba(6, 4, &values);
        let delta = plan_frame_delta(
            &previous,
            &current,
            viewport,
            ViewTransform::default(),
            ViewTransform::default(),
            false,
            1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(delta.damaged_pixels, 4);
        assert!(matches!(
            &delta.operations[0],
            DeltaOperation::Overwrite {
                rect: DamageRect {
                    x: 2,
                    y: 1,
                    width: 2,
                    height: 2
                },
                rgba
            } if rgba.len() == 16
        ));
    }

    #[test]
    fn one_scroll_step_reduces_raw_wire_bytes_by_eighty_six_percent() {
        let width = 800_u64;
        let height = 460_u64;
        let scroll = 60_u64;
        let full_bytes = 96 + width * height * 4;
        let delta_bytes = 48 + 2 * 32 + width * scroll * 4;
        assert_eq!(full_bytes, 1_472_096);
        assert_eq!(delta_bytes, 192_112);
        assert!(delta_bytes * 100 < full_bytes * 14);
    }
}
