//! Run alone with `cargo test --release audit_bench -- --ignored --nocapture --test-threads=1`.
use crate::{
    compositor::{Composer, DeltaOperation, DeltaPlanner, FramePlan, PageImage, ViewTransform},
    geometry::WindowSize,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

struct CountingAllocator;
static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn charge(size: usize) {
    if ENABLED.load(Ordering::Relaxed) {
        ALLOCATED.fetch_add(size, Ordering::Relaxed);
        let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}
// SAFETY: every operation forwards the unchanged allocation contract to System.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller supplied a valid allocation layout.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            charge(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if ENABLED.load(Ordering::Relaxed) {
            let _ = LIVE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
                Some(live.saturating_sub(layout.size()))
            });
        }
        // SAFETY: pointer and layout are the caller's matching allocation.
        unsafe {
            System.dealloc(pointer, layout);
        }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
#[ignore = "isolated performance measurement"]
fn reader_composition_and_delta_workloads() {
    for label in [
        "scroll",
        "page_turn",
        "unchanged",
        "recovery",
        "encoded_scroll",
        "encoded_page_turn",
        "encoded_unchanged",
        "encoded_recovery",
    ] {
        let codec = label.starts_with("encoded_");
        let workload = label.strip_prefix("encoded_").unwrap_or(label);
        ENABLED.store(true, Ordering::Relaxed);
        ALLOCATED.store(0, Ordering::Relaxed);
        LIVE.store(0, Ordering::Relaxed);
        PEAK.store(0, Ordering::Relaxed);
        let viewport = WindowSize::from_cells(80, 24, 10, 20);
        let page = Arc::new(PageImage {
            width: 2040,
            height: 2640,
            row_stride: 2040 * 3,
            pixels: (0..2040 * 2640 * 3)
                .map(|i| ((i / (2040 * 3)) % 251) as u8)
                .collect(),
            highlights: Vec::new(),
        });
        let mut composer = Composer::default();
        let mut planner = DeltaPlanner::default();
        let mut before = ViewTransform::default();
        let mut previous = composer.compose(page.clone(), viewport, before).unwrap();
        let allocated_before = ALLOCATED.load(Ordering::Relaxed);
        let started = Instant::now();
        let mut encoded = 0usize;
        for index in 1..=30 {
            let after = ViewTransform {
                offset_y: if workload == "scroll" { index * 20 } else { 0 },
                ..before
            };
            let next_page = if workload == "page_turn" {
                {
                    let mut next = (*page).clone();
                    next.pixels[0] = index as u8;
                    Arc::new(next)
                }
            } else {
                page.clone()
            };
            let current = composer.compose(next_page, viewport, after).unwrap();
            let plan = planner
                .plan(
                    &previous,
                    &current,
                    viewport,
                    before,
                    after,
                    workload != "page_turn",
                    8,
                )
                .unwrap();
            match plan {
                FramePlan::Unchanged if workload != "recovery" => {}
                FramePlan::Delta(delta) if codec => {
                    encoded += encoded_body_len(&current.rgba, viewport, Some(&delta));
                    planner.recycle(delta);
                }
                FramePlan::Delta(delta) => {
                    encoded += 64
                        + delta
                            .operations
                            .iter()
                            .map(|operation| match operation {
                                DeltaOperation::Copy { .. } => 32,
                                DeltaOperation::Overwrite { rgba, .. } => 32 + rgba.len(),
                            })
                            .sum::<usize>();
                    planner.recycle(delta);
                }
                _ if codec => encoded += encoded_body_len(&current.rgba, viewport, None),
                _ => encoded += current.rgba.len() + 96,
            }
            composer.recycle(previous);
            previous = current;
            before = after;
        }
        let elapsed = started.elapsed();
        let allocated = ALLOCATED.load(Ordering::Relaxed) - allocated_before;
        let peak = PEAK.load(Ordering::Relaxed);
        ENABLED.store(false, Ordering::Relaxed);
        println!(
            "{label}: us/frame={} allocated_bytes={} peak_live_bytes={} record_bytes={encoded}",
            elapsed.as_micros() / 30,
            allocated,
            peak
        );
    }
}

/// Actual raw/zstd codec output for candidate plans, excluding transport framing and the
/// presenter's accumulated-damage policy. Separate runs keep codec allocation out of compose data.
fn encoded_body_len(
    rgba: &[u8],
    viewport: WindowSize,
    delta: Option<&crate::compositor::FrameDelta>,
) -> usize {
    use vivid_protocol::media::{self, RasterDeltaOperation};
    let width = viewport.page_area_width_px();
    let height = viewport.page_area_height_px();
    let encode = |compress| {
        if let Some(delta) = delta {
            let operations: Vec<_> = delta
                .operations
                .iter()
                .map(|operation| match operation {
                    DeltaOperation::Copy {
                        destination_x,
                        destination_y,
                        width,
                        height,
                        source_x,
                        source_y,
                    } => RasterDeltaOperation::Copy {
                        destination_x: *destination_x,
                        destination_y: *destination_y,
                        width: *width,
                        height: *height,
                        source_x: *source_x,
                        source_y: *source_y,
                    },
                    DeltaOperation::Overwrite { rect, rgba } => RasterDeltaOperation::Overwrite {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                        rgba,
                    },
                })
                .collect();
            media::raster_delta_frame_body(1, 2, 1, 0, 0, width, height, 8, &operations, compress)
                .unwrap()
                .len()
        } else {
            media::raster_frame_body_with_compression(1, 2, width, height, rgba, compress)
                .unwrap()
                .len()
        }
    };
    encode(false).min(encode(true))
}

pub(crate) fn begin_measurement() {
    ALLOCATED.store(0, Ordering::Relaxed);
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Relaxed);
}
pub(crate) fn finish_measurement() -> (usize, usize) {
    ENABLED.store(false, Ordering::Relaxed);
    (
        ALLOCATED.load(Ordering::Relaxed),
        PEAK.load(Ordering::Relaxed),
    )
}
