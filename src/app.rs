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
    pub fn text(&self, page: usize, n_pages: usize, zoom_mode: bool) -> String {
        let prefix = format!(
            "{}/{}  {}",
            page + 1,
            n_pages.max(1),
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
    pub visible: bool,
    pub generation: u64,
}

impl App {
    pub fn new(page: usize) -> Self {
        Self {
            page,
            n_pages: 1,
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
            visible: true,
            generation: 1,
        }
    }

    pub fn set_document(&mut self, kind: DocumentKind, n_pages: usize) {
        self.document_kind = kind;
        self.n_pages = n_pages.max(1);
        self.page = self.page.min(self.n_pages - 1);
        self.search_counts.resize(self.n_pages, None);
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
        matches!(self.document_kind, DocumentKind::Fixed)
    }

    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
    }

    pub fn go_to_page(&mut self, page: usize) -> bool {
        if page >= self.n_pages || page == self.page {
            return false;
        }
        self.page = page;
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
        } else if self.page + 1 < self.n_pages {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> WindowSize {
        WindowSize::from_cells(80, 25, 10, 20)
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
    fn search_navigation_wraps() {
        let mut app = App::new(2);
        app.set_document(DocumentKind::Fixed, 4);
        app.search_term = Some("needle".to_owned());
        app.set_search_counts(vec![0, 2, 0, 0]);
        assert!(app.next_search_result(false));
        assert_eq!(app.page, 1);
    }
}
