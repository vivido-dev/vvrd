use anyhow::{Context as _, ensure};
use vivid_sdk::DisplayState;

const DEFAULT_CELL_WIDTH_PX: u32 = 10;
const DEFAULT_CELL_HEIGHT_PX: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

impl WindowSize {
    pub fn current(display: DisplayState) -> anyhow::Result<Self> {
        let (local_cols, local_rows) = crossterm::terminal::size().unwrap_or((0, 0));
        let cols = u16::try_from(display.grid_columns)
            .ok()
            .filter(|value| *value > 0)
            .or_else(|| (local_cols > 0).then_some(local_cols))
            .context("presenter and terminal reported zero columns")?;
        let rows = u16::try_from(display.grid_rows)
            .ok()
            .filter(|value| *value > 1)
            .or_else(|| (local_rows > 1).then_some(local_rows))
            .context("vvrd requires at least two terminal rows")?;

        Ok(Self {
            cols,
            rows,
            cell_width_px: if display.cell_width == 0 {
                DEFAULT_CELL_WIDTH_PX
            } else {
                display.cell_width
            },
            cell_height_px: if display.cell_height == 0 {
                DEFAULT_CELL_HEIGHT_PX
            } else {
                display.cell_height
            },
        })
    }

    pub fn from_cells(cols: u16, rows: u16, cell_width_px: u32, cell_height_px: u32) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(2),
            cell_width_px: cell_width_px.max(1),
            cell_height_px: cell_height_px.max(1),
        }
    }

    pub fn page_rows(self) -> u16 {
        self.rows.saturating_sub(1).max(1)
    }

    pub fn page_area_width_px(self) -> u32 {
        u32::from(self.cols).saturating_mul(self.cell_width_px)
    }

    pub fn page_area_height_px(self) -> u32 {
        u32::from(self.page_rows()).saturating_mul(self.cell_height_px)
    }

    pub fn framebuffer_len(self) -> anyhow::Result<usize> {
        let pixels = u64::from(self.page_area_width_px())
            .checked_mul(u64::from(self.page_area_height_px()))
            .context("viewport pixel count overflow")?;
        let bytes = pixels
            .checked_mul(4)
            .context("viewport byte count overflow")?;
        ensure!(
            bytes <= usize::MAX as u64,
            "viewport buffer exceeds address space"
        );
        Ok(bytes as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_the_last_row_for_status() {
        let size = WindowSize::from_cells(80, 24, 10, 20);
        assert_eq!(size.page_rows(), 23);
        assert_eq!(size.page_area_width_px(), 800);
        assert_eq!(size.page_area_height_px(), 460);
        assert_eq!(size.framebuffer_len().unwrap(), 800 * 460 * 4);
    }

    #[test]
    fn tiny_panes_still_have_a_page_row() {
        let size = WindowSize::from_cells(1, 1, 0, 0);
        assert_eq!(size.rows, 2);
        assert_eq!(size.page_rows(), 1);
    }
}
