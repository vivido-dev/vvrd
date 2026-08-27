use crate::{
    geometry::WindowSize,
    renderer::{DocumentKind, LinkInfo, TocEntry},
};

const ZOOM_STEP: f32 = 1.2;
const MAX_ZOOM_LEVEL: i16 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    GoToPage(String),
    Search(String),
    Toc { selected: usize },
    Metadata,
    Links { links: Vec<LinkInfo>, input: String },
    Help,
}

#[derive(Debug, Clone)]
pub enum StatusMsg {
    Hint,
    Info(String),
}

impl StatusMsg {
    pub fn text(
        &self,
        page: usize,
        n_pages: usize,
        pagination_complete: bool,
        zoom_mode: bool,
    ) -> String {
        let page_count = if pagination_complete {
            n_pages.max(1).to_string()
        } else {
            "?".to_owned()
        };
        let prefix = format!(
            "{}/{}  {}",
            page + 1,
            page_count,
            if zoom_mode { "[ZOOM] " } else { "" }
        );
        match self {
            Self::Hint => {
                format!("{prefix}? help  q quit  ←/→ page  ↑/↓ scroll  i invert  r rotate  z zoom")
            }
            Self::Info(info) => format!("{prefix}{info}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAction {
    Scrolled,
    TurnNext,
    TurnPrev,
    Nothing,
}

pub struct App {
    pub page: usize,
    pub n_pages: usize,
    pub pagination_complete: bool,
    pub document_kind: DocumentKind,
    pub input_mode: InputMode,
    pub msg: StatusMsg,
    pub zoom_mode: bool,
    pub zoom_level: i16,
    pub scroll_y: u32,
    pub pan_x: u32,
    pub rendered_width: u32,
    pub rendered_height: u32,
    pub rotation: u16,
    pub inverted: bool,
    pub tinted: bool,
    pub auto_crop: bool,
    pub epub_font_size: f32,
    pub toc: Vec<TocEntry>,
    pub metadata: Vec<(String, String)>,
    pub search_term: Option<String>,
    pub search_counts: Vec<Option<usize>>,
    pub generation: u64,
}

impl App {
    pub fn new(page: usize) -> Self {
        Self {
            page,
            n_pages: 1,
            pagination_complete: true,
            document_kind: DocumentKind::Fixed,
            input_mode: InputMode::Normal,
            msg: StatusMsg::Hint,
            zoom_mode: false,
            zoom_level: 0,
            scroll_y: 0,
            pan_x: 0,
            rendered_width: 0,
            rendered_height: 0,
            rotation: 0,
            inverted: false,
            tinted: false,
            auto_crop: false,
            epub_font_size: 11.0,
            toc: Vec::new(),
            metadata: Vec::new(),
            search_term: None,
            search_counts: vec![None],
            generation: 1,
        }
    }

    pub fn set_document(&mut self, kind: DocumentKind, n_pages: usize) {
        self.set_document_kind(kind);
        self.n_pages = n_pages.max(1);
        self.pagination_complete = true;
        self.page = self.page.min(self.n_pages - 1);
        self.search_counts.resize(self.n_pages, None);
    }

    pub fn set_document_kind(&mut self, kind: DocumentKind) {
        self.document_kind = kind;
        self.pagination_complete = false;
        if matches!(kind, DocumentKind::Reflowable) {
            self.zoom_mode = false;
            self.zoom_level = 0;
        }
    }

    pub fn zoom_factor(&self) -> f32 {
        ZOOM_STEP.powi(self.zoom_level as i32)
    }

    pub fn render_area(&self, viewport: WindowSize) -> (f32, f32) {
        let zoom = if self.supports_zoom() {
            self.zoom_factor()
        } else {
            1.0
        };
        (
            viewport.page_area_width_px() as f32 * zoom,
            viewport.page_area_height_px() as f32 * zoom,
        )
    }

    pub fn supports_zoom(&self) -> bool {
        matches!(
            self.document_kind,
            DocumentKind::Fixed | DocumentKind::Markdown | DocumentKind::Mermaid
        )
    }

    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
    }

    pub fn go_to_page(&mut self, page: usize) -> bool {
        if page == self.page || self.pagination_complete && page >= self.n_pages {
            return false;
        }
        self.page = page;
        if !self.pagination_complete {
            // Reflowable documents are navigable before MuPDF finishes its whole-book page count.
            // Treat successfully requested pages as a growing lower bound until pagination reports
            // the exact count.
            self.n_pages = self.n_pages.max(page.saturating_add(1));
            self.search_counts.resize(self.n_pages, None);
        }
        self.scroll_y = 0;
        self.pan_x = 0;
        self.invalidate();
        true
    }

    pub fn next_page(&mut self) -> bool {
        self.go_to_page(self.page.saturating_add(1))
    }

    pub fn prev_page(&mut self) -> bool {
        self.page
            .checked_sub(1)
            .is_some_and(|page| self.go_to_page(page))
    }

    pub fn prev_page_at_bottom(&mut self, viewport: WindowSize) -> bool {
        if !self.prev_page() {
            return false;
        }
        self.scroll_y = self.max_scroll_y(viewport);
        true
    }

    pub fn max_scroll_y(&self, viewport: WindowSize) -> u32 {
        self.rendered_height
            .saturating_sub(viewport.page_area_height_px())
    }

    pub fn max_pan_x(&self, viewport: WindowSize) -> u32 {
        self.rendered_width
            .saturating_sub(viewport.page_area_width_px())
    }

    pub fn scroll_down(&mut self, viewport: WindowSize, amount: u32) -> ScrollAction {
        let max = self.max_scroll_y(viewport);
        if self.scroll_y < max {
            self.scroll_y = self.scroll_y.saturating_add(amount).min(max);
            ScrollAction::Scrolled
        } else if !self.pagination_complete || self.page.saturating_add(1) < self.n_pages {
            ScrollAction::TurnNext
        } else {
            ScrollAction::Nothing
        }
    }

    pub fn scroll_up(&mut self, amount: u32) -> ScrollAction {
        if self.scroll_y > 0 {
            self.scroll_y = self.scroll_y.saturating_sub(amount);
            ScrollAction::Scrolled
        } else if self.page > 0 {
            ScrollAction::TurnPrev
        } else {
            ScrollAction::Nothing
        }
    }

    pub fn pan_right(&mut self, viewport: WindowSize, amount: u32) -> bool {
        let old = self.pan_x;
        self.pan_x = self
            .pan_x
            .saturating_add(amount)
            .min(self.max_pan_x(viewport));
        old != self.pan_x
    }

    pub fn pan_left(&mut self, amount: u32) -> bool {
        let old = self.pan_x;
        self.pan_x = self.pan_x.saturating_sub(amount);
        old != self.pan_x
    }

    pub fn toggle_zoom(&mut self) {
        self.zoom_mode = !self.zoom_mode;
        if !self.zoom_mode {
            self.zoom_level = 0;
            self.scroll_y = 0;
            self.pan_x = 0;
        }
        self.invalidate();
    }

    pub fn zoom_in(&mut self) -> bool {
        if self.zoom_level >= MAX_ZOOM_LEVEL {
            return false;
        }
        self.zoom_level += 1;
        self.invalidate();
        true
    }

    pub fn zoom_out(&mut self, viewport: WindowSize) -> bool {
        if self.zoom_level <= 0 {
            return false;
        }
        self.zoom_level -= 1;
        self.scroll_y = self.scroll_y.min(self.max_scroll_y(viewport));
        self.pan_x = self.pan_x.min(self.max_pan_x(viewport));
        self.invalidate();
        true
    }

    /// Carry the view offsets across a viewport change in proportion to the page area.
    ///
    /// `scroll_y` and `pan_x` index the rendered page in pixels, and the rendered page is sized
    /// from the page area, so a cell-metric change alone rescales the page underneath fixed
    /// offsets. Clamping in [`Self::set_rendered_size`] cannot stand in for this: the maxima grow
    /// with the page, so an offset that is now too small is never corrected and the reader
    /// silently moves toward the top of a zoomed page. A terminal font-size change is the ordinary
    /// way to hit that.
    pub fn rescale_offsets(&mut self, old: WindowSize, new: WindowSize) {
        self.scroll_y = rescale_offset(
            self.scroll_y,
            old.page_area_height_px(),
            new.page_area_height_px(),
        );
        self.pan_x = rescale_offset(
            self.pan_x,
            old.page_area_width_px(),
            new.page_area_width_px(),
        );
    }

    pub fn set_rendered_size(&mut self, width: u32, height: u32, viewport: WindowSize) {
        self.rendered_width = width;
        self.rendered_height = height;
        self.scroll_y = self.scroll_y.min(self.max_scroll_y(viewport));
        self.pan_x = self.pan_x.min(self.max_pan_x(viewport));
    }

    pub fn clear_search_results(&mut self) {
        self.search_counts.clear();
        self.search_counts.resize(self.n_pages, None);
    }

    pub fn set_search_counts(&mut self, counts: Vec<usize>) {
        self.search_counts = counts.into_iter().map(Some).collect();
        self.search_counts.resize(self.n_pages, Some(0));
    }

    pub fn next_search_result(&mut self, reverse: bool) -> bool {
        if self.search_term.is_none() || self.n_pages == 0 {
            return false;
        }
        for offset in 1..=self.n_pages {
            let page = if reverse {
                (self.page + self.n_pages - (offset % self.n_pages)) % self.n_pages
            } else {
                (self.page + offset) % self.n_pages
            };
            if self
                .search_counts
                .get(page)
                .and_then(|count| *count)
                .unwrap_or(0)
                > 0
            {
                return self.go_to_page(page);
            }
        }
        false
    }

    pub fn show_info(&mut self, message: impl Into<String>) {
        self.msg = StatusMsg::Info(message.into());
    }
}

/// Move one pixel offset between two page-area extents, widening so the product cannot wrap.
fn rescale_offset(offset: u32, from: u32, to: u32) -> u32 {
    if offset == 0 || from == 0 || from == to {
        return offset;
    }
    u32::try_from(u64::from(offset) * u64::from(to) / u64::from(from)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> WindowSize {
        WindowSize::from_cells(80, 25, 10, 20)
    }

    #[test]
    fn a_cell_metric_change_carries_view_offsets_proportionally() {
        // Only the cell size changes here, not the cell count: the grid stays 101x53 while the
        // pixels behind it grow tenfold. That is what a terminal font-size change does, and what
        // a scaled capture would do deliberately. The rendered page grows with the page area, so
        // offsets left at their old pixel values would put the reader ten times nearer the top,
        // and the growing maximum would never clamp them back.
        let small = WindowSize::from_cells(101, 53, 3, 9);
        let large = WindowSize::from_cells(101, 53, 30, 90);
        assert_eq!(
            (
                large.page_area_width_px() / small.page_area_width_px(),
                large.page_area_height_px() / small.page_area_height_px(),
            ),
            (10, 10),
            "the fixture must scale the page area uniformly"
        );

        let mut app = App::new(0);
        app.set_document(DocumentKind::Fixed, 1);
        app.zoom_mode = true;
        assert!(app.zoom_in());
        app.set_rendered_size(606, 936, small);
        app.scroll_y = 234;
        app.pan_x = 152;
        let before = (
            f64::from(app.scroll_y) / f64::from(app.rendered_height),
            f64::from(app.pan_x) / f64::from(app.rendered_width),
        );

        app.rescale_offsets(small, large);
        app.set_rendered_size(6060, 9360, large);

        assert_eq!((app.scroll_y, app.pan_x), (2340, 1520));
        let after = (
            f64::from(app.scroll_y) / f64::from(app.rendered_height),
            f64::from(app.pan_x) / f64::from(app.rendered_width),
        );
        assert!(
            (after.0 - before.0).abs() < 1e-9 && (after.1 - before.1).abs() < 1e-9,
            "the visible region moved: {before:?} became {after:?}"
        );
    }

    #[test]
    fn an_unzoomed_page_has_no_offsets_to_carry() {
        // At default zoom the rendered page is exactly the page area, so both maxima are zero and
        // both offsets stay pinned. This is why a scaled capture of an unzoomed page is safe.
        let small = WindowSize::from_cells(101, 53, 3, 9);
        let large = WindowSize::from_cells(101, 53, 30, 90);

        let mut app = App::new(0);
        app.set_document(DocumentKind::Fixed, 1);
        app.set_rendered_size(
            small.page_area_width_px(),
            small.page_area_height_px(),
            small,
        );
        assert_eq!((app.max_scroll_y(small), app.max_pan_x(small)), (0, 0));

        app.rescale_offsets(small, large);
        app.set_rendered_size(
            large.page_area_width_px(),
            large.page_area_height_px(),
            large,
        );

        assert_eq!((app.scroll_y, app.pan_x), (0, 0));
    }

    #[test]
    fn offset_rescaling_is_saturating_and_leaves_degenerate_inputs_alone() {
        assert_eq!(rescale_offset(0, 468, 4680), 0);
        assert_eq!(rescale_offset(234, 0, 4680), 234);
        assert_eq!(rescale_offset(234, 468, 468), 234);
        assert_eq!(rescale_offset(4680, 4680, 468), 468);
        assert_eq!(rescale_offset(u32::MAX, 1, u32::MAX), u32::MAX);
    }

    #[test]
    fn navigation_resets_view_offsets() {
        let mut app = App::new(0);
        app.set_document(DocumentKind::Fixed, 3);
        app.scroll_y = 10;
        app.pan_x = 20;
        assert!(app.next_page());
        assert_eq!((app.page, app.scroll_y, app.pan_x), (1, 0, 0));
    }

    #[test]
    fn scrolling_turns_at_page_boundaries() {
        let mut app = App::new(0);
        app.set_document(DocumentKind::Fixed, 2);
        app.set_rendered_size(800, 900, viewport());
        assert_eq!(app.scroll_down(viewport(), 1000), ScrollAction::Scrolled);
        assert_eq!(app.scroll_down(viewport(), 10), ScrollAction::TurnNext);
        assert!(app.next_page());
        assert_eq!(app.scroll_up(10), ScrollAction::TurnPrev);
    }

    #[test]
    fn reflowable_documents_disable_zoom() {
        let mut app = App::new(0);
        app.zoom_mode = true;
        app.zoom_level = 3;
        app.set_document(DocumentKind::Reflowable, 10);
        assert!(!app.zoom_mode);
        assert_eq!(app.zoom_level, 0);
    }

    #[test]
    fn reflowable_page_count_stays_unknown_until_pagination_completes() {
        let mut app = App::new(999);
        app.set_document_kind(DocumentKind::Reflowable);
        assert!(!app.pagination_complete);
        assert_eq!(app.page, 999);
        assert!(
            app.msg
                .text(app.page, app.n_pages, false, false)
                .starts_with("1000/?")
        );

        app.set_document(DocumentKind::Reflowable, 962);
        assert!(app.pagination_complete);
        assert_eq!(app.page, 961);
        assert!(
            app.msg
                .text(app.page, app.n_pages, true, false)
                .starts_with("962/962")
        );
    }

    #[test]
    fn reflowable_navigation_does_not_wait_for_page_count() {
        let mut app = App::new(0);
        app.set_document_kind(DocumentKind::Reflowable);

        assert!(app.next_page());
        assert_eq!(app.page, 1);
        assert_eq!(app.n_pages, 2);
        assert_eq!(app.scroll_down(viewport(), 1_000), ScrollAction::TurnNext);
        assert!(app.next_page());
        assert_eq!(app.page, 2);
        assert!(!app.pagination_complete);
    }

    #[test]
    fn search_navigation_wraps() {
        let mut app = App::new(2);
        app.set_document(DocumentKind::Fixed, 4);
        app.search_term = Some("needle".to_owned());
        app.set_search_counts(vec![0, 2, 0, 0]);
        assert!(app.next_search_result(false));
        assert_eq!(app.page, 1);
    }
}
