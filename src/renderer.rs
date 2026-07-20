use std::{
    any::Any,
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use flume::{Receiver, Sender};
use mupdf::{
    Colorspace, Document, Matrix, MetadataName, Page, Quad, TextPageFlags,
    text_page::SearchHitResponse,
};

use crate::{
    compositor::{HighlightRect, PageImage, crop_whitespace_image},
    error::RenderError,
    geometry::WindowSize,
};

const MAX_RENDER_DIMENSION: f32 = 16_384.0;
const EPUB_LAYOUT_MIN_W: f32 = 260.0;
const EPUB_LAYOUT_MAX_W: f32 = 396.0;
const EPUB_LAYOUT_ASPECT: f32 = 595.0 / 420.0;
const SLOW_RENDER_WARN: Duration = Duration::from_secs(5);
pub const MUPDF_BLACK: i32 = 0;
pub const MUPDF_WHITE: i32 = i32::from_be_bytes([0, 0xff, 0xff, 0xff]);
pub const TINT_BLACK: i32 = i32::from_be_bytes([0, 0x70, 0x42, 0x14]);
pub const TINT_WHITE: i32 = i32::from_be_bytes([0, 0xF5, 0xE6, 0xC8]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Fixed,
    Reflowable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInfo {
    pub text: String,
    pub uri: String,
    pub page: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    pub title: String,
    pub page: usize,
    pub level: usize,
}

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub width_px: f32,
    pub height_px: f32,
    pub rotation: u16,
    pub inverted: bool,
    pub tinted: bool,
    pub black: i32,
    pub white: i32,
    pub epub_font_size: f32,
    pub search_term: Option<String>,
    pub generation: u64,
}

impl RenderOptions {
    pub fn for_viewport(viewport: WindowSize, generation: u64) -> Self {
        Self {
            width_px: viewport.page_area_width_px() as f32,
            height_px: viewport.page_area_height_px() as f32,
            rotation: 0,
            inverted: false,
            tinted: false,
            black: MUPDF_BLACK,
            white: MUPDF_WHITE,
            epub_font_size: 11.0,
            search_term: None,
            generation,
        }
    }
}

pub enum RenderCmd {
    Render {
        page: usize,
        options: RenderOptions,
    },
    Search(String),
    ClearCache,
    GetLinks(usize),
    Export {
        page: usize,
        output: PathBuf,
        options: RenderOptions,
        auto_crop: bool,
    },
    Shutdown,
}

pub enum RenderEvent {
    Opened {
        kind: DocumentKind,
        n_pages: usize,
        toc: Vec<TocEntry>,
        metadata: Vec<(String, String)>,
    },
    Page {
        page: usize,
        generation: u64,
        image: PageImage,
    },
    SearchComplete(Vec<usize>),
    Links(Vec<LinkInfo>),
    Exported(PathBuf),
    Notice(String),
    Error(String),
    Stopped,
}

pub struct RenderThread {
    pub commands: Sender<RenderCmd>,
    pub events: Receiver<RenderEvent>,
    join: Option<JoinHandle<()>>,
}

impl RenderThread {
    pub fn spawn(path: PathBuf, viewport: WindowSize) -> Self {
        let (commands, command_rx) = flume::unbounded();
        let (event_tx, events) = flume::unbounded();
        let join = thread::Builder::new()
            .name("vvrd-render".to_owned())
            .spawn(move || run_render_thread(path, viewport, command_rx, event_tx))
            .expect("failed to spawn document render thread");
        Self {
            commands,
            events,
            join: Some(join),
        }
    }

    pub fn shutdown(mut self) {
        let _ = self.commands.send(RenderCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        let _ = self.commands.send(RenderCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run_render_thread(
    path: PathBuf,
    viewport: WindowSize,
    commands: Receiver<RenderCmd>,
    events: Sender<RenderEvent>,
) {
    let mut document = match open_document(&path, viewport, 11.0) {
        Ok(document) => document,
        Err(error) => {
            let _ = events.send(RenderEvent::Error(error.to_string()));
            let _ = events.send(RenderEvent::Stopped);
            return;
        }
    };
    let mut kind = document_kind(&document);
    let mut layout = matches!(kind, DocumentKind::Reflowable).then_some((
        viewport.page_area_width_px() as f32,
        viewport.page_area_height_px() as f32,
        11.0,
    ));
    if send_opened(&document, kind, &events).is_err() {
        return;
    }

    let mut cache = RenderCache::default();
    let mut deferred = VecDeque::new();
    let heartbeat = RenderHeartbeat::start(events.clone());
    while let Some(command) = next_render_command(&commands, &mut deferred) {
        let mut prerender_request = None;
        let result = match command {
            RenderCmd::Render { page, options } => {
                if matches!(kind, DocumentKind::Reflowable) {
                    let next = (options.width_px, options.height_px, options.epub_font_size);
                    if layout != Some(next) {
                        let (width, height, em) = epub_layout_for_area(next.0, next.1, next.2);
                        match document.layout(width, height, em) {
                            Ok(()) => {
                                layout = Some(next);
                                cache.clear();
                                kind = document_kind(&document);
                                let _ = send_opened(&document, kind, &events);
                            }
                            Err(error) => {
                                let _ = events.send(RenderEvent::Error(error.to_string()));
                                continue;
                            }
                        }
                    }
                }
                let key = CacheKey::new(page, &options);
                let image = if let Some(image) = cache.get(&key) {
                    Ok(image)
                } else {
                    render_with_isolation(&document, page, &options, &heartbeat).inspect(|image| {
                        cache.insert(key, image.clone());
                    })
                };
                let event = image.map(|image| RenderEvent::Page {
                    page,
                    generation: options.generation,
                    image,
                });
                if event.is_ok() {
                    prerender_request = Some((page, options));
                }
                event
            }
            RenderCmd::Search(term) => {
                search_document(&document, &term).map(RenderEvent::SearchComplete)
            }
            RenderCmd::ClearCache => {
                cache.clear();
                continue;
            }
            RenderCmd::GetLinks(page) => Ok(RenderEvent::Links(extract_links(&document, page))),
            RenderCmd::Export {
                page,
                output,
                options,
                auto_crop,
            } => export_page(&document, page, &options, &output).and_then(|()| {
                if auto_crop {
                    let image = render_loaded_page(&document, page, &options)?
                        .into_rgb()
                        .map_err(|error| RenderError::Converting(error.to_string()))?;
                    crop_whitespace_image(image)
                        .save_with_format(&output, image::ImageFormat::Png)
                        .map_err(|error| RenderError::Converting(error.to_string()))?;
                }
                Ok(RenderEvent::Exported(output))
            }),
            RenderCmd::Shutdown => break,
        };
        match result {
            Ok(event) => {
                if events.send(event).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = events.send(RenderEvent::Error(error.to_string()));
            }
        }
        if let Some((page, options)) = prerender_request
            && commands.is_empty()
        {
            prerender_neighbors(&document, page, &options, &mut cache);
        }
    }
    heartbeat.stop();
    let _ = events.send(RenderEvent::Stopped);
}

struct RenderHeartbeat {
    base: Instant,
    started_ms: AtomicU64,
    page: AtomicUsize,
    active: AtomicBool,
    warned: AtomicBool,
    stopped: AtomicBool,
}

impl RenderHeartbeat {
    fn start(events: Sender<RenderEvent>) -> Arc<Self> {
        let heartbeat = Arc::new(Self {
            base: Instant::now(),
            started_ms: AtomicU64::new(0),
            page: AtomicUsize::new(0),
            active: AtomicBool::new(false),
            warned: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        let watcher = Arc::clone(&heartbeat);
        thread::Builder::new()
            .name("vvrd-render-watchdog".to_owned())
            .spawn(move || {
                while !watcher.stopped.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(250));
                    if !watcher.active.load(Ordering::Acquire) {
                        continue;
                    }
                    let elapsed = watcher
                        .base
                        .elapsed()
                        .as_millis()
                        .saturating_sub(watcher.started_ms.load(Ordering::Relaxed) as u128);
                    if elapsed >= SLOW_RENDER_WARN.as_millis()
                        && !watcher.warned.swap(true, Ordering::AcqRel)
                    {
                        let _ = events.send(RenderEvent::Notice(format!(
                            "Rendering page {} is taking a while...",
                            watcher.page.load(Ordering::Relaxed) + 1
                        )));
                    }
                }
            })
            .expect("failed to spawn render watchdog");
        heartbeat
    }

    fn begin(&self, page: usize) {
        self.page.store(page, Ordering::Relaxed);
        self.started_ms
            .store(self.base.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.warned.store(false, Ordering::Release);
        self.active.store(true, Ordering::Release);
    }

    fn end(&self) {
        self.active.store(false, Ordering::Release);
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
}

fn render_with_isolation(
    document: &Document,
    page: usize,
    options: &RenderOptions,
    heartbeat: &RenderHeartbeat,
) -> Result<PageImage, RenderError> {
    heartbeat.begin(page);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render_loaded_page(document, page, options)
    }));
    heartbeat.end();
    match result {
        Ok(result) => result,
        Err(panic) => Err(RenderError::Panicked {
            page,
            message: panic_message(&*panic),
        }),
    }
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    page: usize,
    width: u32,
    height: u32,
    rotation: u16,
    inverted: bool,
    tinted: bool,
    black: i32,
    white: i32,
    epub_font_size: u32,
    search_term: Option<String>,
}

impl CacheKey {
    fn new(page: usize, options: &RenderOptions) -> Self {
        Self {
            page,
            width: options.width_px.to_bits(),
            height: options.height_px.to_bits(),
            rotation: options.rotation,
            inverted: options.inverted,
            tinted: options.tinted,
            black: options.black,
            white: options.white,
            epub_font_size: options.epub_font_size.to_bits(),
            search_term: options.search_term.clone(),
        }
    }
}

#[derive(Default)]
struct RenderCache {
    pages: HashMap<CacheKey, PageImage>,
    order: VecDeque<CacheKey>,
    bytes: usize,
}

impl RenderCache {
    const MAX_PAGES: usize = 24;
    const MAX_BYTES: usize = 256 * 1024 * 1024;

    fn get(&mut self, key: &CacheKey) -> Option<PageImage> {
        let image = self.pages.get(key)?.clone();
        self.order.retain(|candidate| candidate != key);
        self.order.push_back(key.clone());
        Some(image)
    }

    fn insert(&mut self, key: CacheKey, image: PageImage) {
        if let Some(previous) = self.pages.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous.pixels.len());
            self.order.retain(|candidate| candidate != &key);
        }
        self.bytes = self.bytes.saturating_add(image.pixels.len());
        self.pages.insert(key.clone(), image);
        self.order.push_back(key);
        while self.pages.len() > Self::MAX_PAGES || self.bytes > Self::MAX_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(image) = self.pages.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(image.pixels.len());
            }
        }
    }

    fn clear(&mut self) {
        self.pages.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

fn prerender_neighbors(
    document: &Document,
    page: usize,
    options: &RenderOptions,
    cache: &mut RenderCache,
) {
    let count = usize::try_from(document.page_count().unwrap_or(0)).unwrap_or(0);
    for neighbor in [
        page.checked_add(1).filter(|value| *value < count),
        page.checked_sub(1),
    ]
    .into_iter()
    .flatten()
    {
        let key = CacheKey::new(neighbor, options);
        if cache.pages.contains_key(&key) {
            continue;
        }
        if let Ok(image) = render_loaded_page(document, neighbor, options) {
            cache.insert(key, image);
        }
    }
}

fn next_render_command(
    commands: &Receiver<RenderCmd>,
    deferred: &mut VecDeque<RenderCmd>,
) -> Option<RenderCmd> {
    let mut command = deferred.pop_front().or_else(|| commands.recv().ok())?;
    if matches!(command, RenderCmd::Render { .. }) {
        while let Ok(next) = commands.try_recv() {
            match next {
                RenderCmd::Render { .. } => command = next,
                other => {
                    deferred.push_back(other);
                    break;
                }
            }
        }
    }
    Some(command)
}

fn open_document(
    path: &Path,
    viewport: WindowSize,
    epub_font_size: f32,
) -> Result<Document, RenderError> {
    #[cfg(windows)]
    let path = path.to_string_lossy();
    #[cfg_attr(unix, allow(clippy::borrow_deref_ref))]
    let mut document = Document::open(&*path)?;
    if document.is_reflowable().unwrap_or(false) {
        let (width, height, em) = epub_layout_for_area(
            viewport.page_area_width_px() as f32,
            viewport.page_area_height_px() as f32,
            epub_font_size,
        );
        document.layout(width, height, em)?;
    }
    if document.page_count()? <= 0 {
        return Err(RenderError::EmptyDocument);
    }
    Ok(document)
}

fn document_kind(document: &Document) -> DocumentKind {
    if document.is_reflowable().unwrap_or(false) {
        DocumentKind::Reflowable
    } else {
        DocumentKind::Fixed
    }
}

fn send_opened(
    document: &Document,
    kind: DocumentKind,
    events: &Sender<RenderEvent>,
) -> Result<(), flume::SendError<RenderEvent>> {
    let n_pages = usize::try_from(document.page_count().unwrap_or(0))
        .unwrap_or(0)
        .max(1);
    let mut toc = Vec::new();
    if let Ok(outlines) = document.outlines() {
        flatten_outlines(&outlines, 0, &mut toc);
    }
    events.send(RenderEvent::Opened {
        kind,
        n_pages,
        toc,
        metadata: extract_metadata(document),
    })
}

pub struct RenderedDocument {
    pub page: PageImage,
    pub page_num: usize,
    pub n_pages: usize,
}

pub fn render_page(
    path: &Path,
    requested_page: usize,
    viewport: WindowSize,
) -> Result<RenderedDocument, RenderError> {
    let document = open_document(path, viewport, 11.0)?;
    let n_pages = usize::try_from(document.page_count()?).unwrap_or(0);
    if n_pages == 0 {
        return Err(RenderError::EmptyDocument);
    }
    let page_num = requested_page.min(n_pages - 1);
    let page = render_loaded_page(
        &document,
        page_num,
        &RenderOptions::for_viewport(viewport, 1),
    )?;
    Ok(RenderedDocument {
        page,
        page_num,
        n_pages,
    })
}

pub fn export_document_page(
    path: &Path,
    page: usize,
    viewport: WindowSize,
    options: &RenderOptions,
    output: &Path,
    auto_crop: bool,
) -> Result<(), RenderError> {
    let document = open_document(path, viewport, options.epub_font_size)?;
    if auto_crop {
        let image = render_loaded_page(&document, page, options)?
            .into_rgb()
            .map_err(|error| RenderError::Converting(error.to_string()))?;
        crop_whitespace_image(image)
            .save_with_format(output, image::ImageFormat::Png)
            .map_err(|error| RenderError::Converting(error.to_string()))
    } else {
        export_page(&document, page, options, output)
    }
}

pub fn document_page_count(
    path: &Path,
    viewport: WindowSize,
    epub_font_size: f32,
) -> Result<usize, RenderError> {
    let document = open_document(path, viewport, epub_font_size)?;
    Ok(usize::try_from(document.page_count()?).unwrap_or(0))
}

fn render_loaded_page(
    document: &Document,
    page_num: usize,
    options: &RenderOptions,
) -> Result<PageImage, RenderError> {
    let count = usize::try_from(document.page_count()?).unwrap_or(0);
    if count == 0 {
        return Err(RenderError::EmptyDocument);
    }
    let page = document.load_page(page_num.min(count - 1) as i32)?;
    let bounds = page.bounds()?;
    let natural_width = (bounds.x1 - bounds.x0).max(1.0);
    let natural_height = (bounds.y1 - bounds.y0).max(1.0);
    let rotated = options.rotation % 180 != 0;
    let dimensions = if rotated {
        (natural_height, natural_width)
    } else {
        (natural_width, natural_height)
    };
    let (_, _, scale) = scale_fit(dimensions, (options.width_px, options.height_px));
    let mut matrix = Matrix::new_scale(scale, scale);
    matrix.rotate((options.rotation % 360) as f32);
    let mut pixmap = page.to_pixmap(&matrix, &Colorspace::device_rgb(), false, false)?;
    let (black, white) = if options.tinted {
        (TINT_BLACK, TINT_WHITE)
    } else {
        (options.black, options.white)
    };
    if options.inverted {
        pixmap.tint(white, black)?;
    } else if black != MUPDF_BLACK || white != MUPDF_WHITE {
        pixmap.tint(black, white)?;
    }
    let highlights = search_page(&page, options.search_term.as_deref())?
        .into_iter()
        .map(|quad| highlight_rect(quad, scale))
        .collect();
    copy_pixmap_rgb(&pixmap, highlights)
}

fn copy_pixmap_rgb(
    pixmap: &mupdf::Pixmap,
    highlights: Vec<HighlightRect>,
) -> Result<PageImage, RenderError> {
    let width = pixmap.width();
    let height = pixmap.height();
    let components = usize::from(pixmap.n());
    let row_stride = width as usize * 3;
    let pixels = match components {
        3 => pixmap.samples().to_vec(),
        4 => {
            let mut rgb = Vec::with_capacity(row_stride * height as usize);
            for pixel in pixmap.samples().chunks_exact(4) {
                rgb.extend_from_slice(&pixel[..3]);
            }
            rgb
        }
        other => {
            return Err(RenderError::Converting(format!(
                "unsupported MuPDF pixmap with {other} components"
            )));
        }
    };
    Ok(PageImage {
        pixels,
        width,
        height,
        row_stride,
        highlights,
    })
}

fn search_page(page: &Page, term: Option<&str>) -> Result<Vec<Quad>, mupdf::error::Error> {
    term.filter(|term| !term.is_empty())
        .map(|term| {
            page.to_text_page(TextPageFlags::empty()).and_then(|text| {
                let mut results = Vec::new();
                text.search_cb(term, &mut results, |results, hits| {
                    results.extend(hits.iter().cloned());
                    SearchHitResponse::ContinueSearch
                })
                .map(|_| results)
            })
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn search_document(document: &Document, term: &str) -> Result<Vec<usize>, RenderError> {
    let count = usize::try_from(document.page_count()?).unwrap_or(0);
    let mut counts = Vec::with_capacity(count);
    for page_num in 0..count {
        let page = document.load_page(page_num as i32)?;
        counts.push(search_page(&page, Some(term))?.len());
    }
    Ok(counts)
}

fn highlight_rect(quad: Quad, scale: f32) -> HighlightRect {
    HighlightRect {
        x0: (quad.ul.x * scale).max(0.0) as u32,
        y0: (quad.ul.y * scale).max(0.0) as u32,
        x1: (quad.lr.x * scale).max(0.0) as u32,
        y1: (quad.lr.y * scale).max(0.0) as u32,
    }
}

fn export_page(
    document: &Document,
    page: usize,
    options: &RenderOptions,
    output: &Path,
) -> Result<(), RenderError> {
    let image = render_loaded_page(document, page, options)?
        .into_rgb()
        .map_err(|error| RenderError::Converting(error.to_string()))?;
    image
        .save_with_format(output, image::ImageFormat::Png)
        .map_err(|error| {
            RenderError::Converting(format!("cannot write {}: {error}", output.display()))
        })
}

fn extract_metadata(document: &Document) -> Vec<(String, String)> {
    let keys = [
        ("Format", MetadataName::Format),
        ("Encryption", MetadataName::Encryption),
        ("Title", MetadataName::Title),
        ("Author", MetadataName::Author),
        ("Subject", MetadataName::Subject),
        ("Keywords", MetadataName::Keywords),
        ("Creator", MetadataName::Creator),
        ("Producer", MetadataName::Producer),
        ("Creation Date", MetadataName::CreationDate),
        ("Modification Date", MetadataName::ModDate),
    ];
    keys.into_iter()
        .filter_map(|(label, key)| document.metadata(key).ok().map(|value| (label, value)))
        .filter_map(|(label, value)| {
            let value = value.trim().to_owned();
            (!value.is_empty()).then(|| (label.to_owned(), value))
        })
        .collect()
}

fn extract_links(document: &Document, page_num: usize) -> Vec<LinkInfo> {
    let Ok(page) = document.load_page(page_num as i32) else {
        return Vec::new();
    };
    let Ok(links) = page.links() else {
        return Vec::new();
    };
    links
        .map(|link| {
            let page = link
                .dest
                .as_ref()
                .map(|destination| destination.loc.page_number as usize);
            LinkInfo {
                text: page.map_or_else(|| link.uri.clone(), |page| format!("Page {}", page + 1)),
                uri: link.uri.clone(),
                page,
            }
        })
        .collect()
}

fn flatten_outlines(outlines: &[mupdf::Outline], level: usize, output: &mut Vec<TocEntry>) {
    for outline in outlines {
        output.push(TocEntry {
            title: outline.title.clone(),
            page: outline
                .dest
                .as_ref()
                .map(|destination| destination.loc.page_number as usize)
                .unwrap_or(0),
            level,
        });
        flatten_outlines(&outline.down, level + 1, output);
    }
}

fn scale_fit((width, height): (f32, f32), (area_w, area_h): (f32, f32)) -> (f32, f32, f32) {
    let mut scale = (area_w / width).min(area_h / height).max(f32::EPSILON);
    let projected_w = width * scale;
    let projected_h = height * scale;
    if projected_w > MAX_RENDER_DIMENSION || projected_h > MAX_RENDER_DIMENSION {
        scale /= (projected_w / MAX_RENDER_DIMENSION).max(projected_h / MAX_RENDER_DIMENSION);
    }
    (width * scale, height * scale, scale)
}

fn epub_layout_for_area(area_w_px: f32, area_h_px: f32, em: f32) -> (f32, f32, f32) {
    let layout_width = (area_w_px * 0.45).clamp(EPUB_LAYOUT_MIN_W, EPUB_LAYOUT_MAX_W);
    let layout_height = (layout_width * EPUB_LAYOUT_ASPECT).min(area_h_px.max(layout_width));
    (layout_width, layout_height, em.clamp(9.0, 18.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_fit_preserves_aspect_ratio() {
        let (width, height, scale) = scale_fit((600.0, 800.0), (1200.0, 800.0));
        assert_eq!((width, height), (600.0, 800.0));
        assert_eq!(scale, 1.0);
    }

    #[test]
    fn scale_fit_clamps_extreme_dimensions() {
        let (width, height, _) = scale_fit((1.0, 1.0), (100_000.0, 100_000.0));
        assert!(width <= MAX_RENDER_DIMENSION);
        assert!(height <= MAX_RENDER_DIMENSION);
    }

    #[test]
    fn epub_layout_is_book_shaped_and_bounded() {
        let (width, height, em) = epub_layout_for_area(1200.0, 800.0, 30.0);
        assert!((EPUB_LAYOUT_MIN_W..=EPUB_LAYOUT_MAX_W).contains(&width));
        assert!(height > width);
        assert_eq!(em, 18.0);
    }

    #[test]
    fn epub_serif_fallback_is_compiled_in() {
        // MuPDF's HTML engine falls back from Charis SIL to the Base-14 Times face through its
        // built-in-only path. A system font or FontLoader cannot satisfy this particular lookup.
        let font = mupdf::Font::new("Times-Roman").expect("Base-14 EPUB fallback is unavailable");
        assert_ne!(font.encode_character('A' as i32).unwrap(), 0);
    }

    #[test]
    fn render_cache_is_page_bounded() {
        let mut cache = RenderCache::default();
        let options = RenderOptions::for_viewport(WindowSize::from_cells(80, 24, 10, 20), 1);
        for page in 0..RenderCache::MAX_PAGES + 3 {
            cache.insert(
                CacheKey::new(page, &options),
                PageImage {
                    pixels: vec![0; 3],
                    width: 1,
                    height: 1,
                    row_stride: 3,
                    highlights: Vec::new(),
                },
            );
        }
        assert_eq!(cache.pages.len(), RenderCache::MAX_PAGES);
        assert!(!cache.pages.contains_key(&CacheKey::new(0, &options)));
    }

    #[test]
    fn render_requests_coalesce_without_crossing_queries() {
        let (sender, receiver) = flume::unbounded();
        let viewport = WindowSize::from_cells(80, 24, 10, 20);
        for generation in [1, 2] {
            sender
                .send(RenderCmd::Render {
                    page: generation as usize,
                    options: RenderOptions::for_viewport(viewport, generation),
                })
                .unwrap();
        }
        sender.send(RenderCmd::GetLinks(2)).unwrap();
        sender
            .send(RenderCmd::Render {
                page: 3,
                options: RenderOptions::for_viewport(viewport, 3),
            })
            .unwrap();
        let mut deferred = VecDeque::new();
        assert!(matches!(
            next_render_command(&receiver, &mut deferred),
            Some(RenderCmd::Render { page: 2, .. })
        ));
        assert!(matches!(
            next_render_command(&receiver, &mut deferred),
            Some(RenderCmd::GetLinks(2))
        ));
        assert!(matches!(
            next_render_command(&receiver, &mut deferred),
            Some(RenderCmd::Render { page: 3, .. })
        ));
    }
}
