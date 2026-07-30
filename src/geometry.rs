use std::io;

use anyhow::{Context as _, ensure};
use vivid_protocol::messages::PayloadMap;

const DEFAULT_CELL_WIDTH_PX: u32 = 10;
const DEFAULT_CELL_HEIGHT_PX: u32 = 20;

/// Terminal metrics reported by the presenter's `terminal-surface-v1` target descriptor.
///
/// Vivid 1.5 core has no grid: cell geometry is a property of the selected presentation target
/// profile, and arrives in `WELCOME`/`TARGET_CHANGED` rather than on any source or surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

impl WindowSize {
    /// Read the terminal target descriptor, returning the viewport and whether it has settled.
    ///
    /// Unsettled geometry is returned rather than rejected: vvrd follows a drag with a scene-node
    /// update and only replaces its raster track once the target settles.
    pub fn from_target_descriptor(descriptor: &PayloadMap) -> io::Result<(Self, bool)> {
        if descriptor.len() != 9
            || descriptor
                .iter()
                .enumerate()
                .any(|(index, (key, _))| *key != index as u64)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal target descriptor must contain exactly keys 0 through 8",
            ));
        }
        let unsigned = |key: usize| {
            descriptor[key].1.as_u64().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("terminal target descriptor key {key} is not unsigned"),
                )
            })
        };
        let settled = descriptor[6].1.as_bool().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal target descriptor settled flag is not boolean",
            )
        })?;
        let cols = u16::try_from(unsigned(2)?).unwrap_or(u16::MAX);
        let rows = u16::try_from(unsigned(3)?).unwrap_or(u16::MAX);
        let cell_width = u32::try_from(unsigned(4)?).unwrap_or(u32::MAX);
        let cell_height = u32::try_from(unsigned(5)?).unwrap_or(u32::MAX);
        if unsigned(0)? == 0
            || unsigned(1)? == 0
            || cols == 0
            || rows == 0
            || cell_width == 0
            || cell_height == 0
            || unsigned(7)? != 3
            || unsigned(8)? == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal target descriptor contains invalid dimensions or anchor capabilities",
            ));
        }
        if rows < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "vvrd requires at least two terminal rows",
            ));
        }
        Ok((
            Self::from_cells(cols, rows, cell_width, cell_height),
            settled,
        ))
    }

    /// Local terminal fallback for presenters that report an unusable target descriptor.
    pub fn from_terminal() -> anyhow::Result<Self> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
        ensure!(cols > 0, "presenter and terminal reported zero columns");
        ensure!(rows > 1, "vvrd requires at least two terminal rows");
        Ok(Self::from_cells(
            cols,
            rows,
            DEFAULT_CELL_WIDTH_PX,
            DEFAULT_CELL_HEIGHT_PX,
        ))
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
    use vivid_protocol::cbor::Value;

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

    #[test]
    fn unsettled_geometry_is_reported_rather_than_rejected() {
        let mut descriptor = vec![
            (0, Value::Unsigned(900)),
            (1, Value::Unsigned(600)),
            (2, Value::Unsigned(90)),
            (3, Value::Unsigned(30)),
            (4, Value::Unsigned(10)),
            (5, Value::Unsigned(20)),
            (6, Value::Bool(false)),
            (7, Value::Unsigned(3)),
            (8, Value::Unsigned(64)),
        ];
        assert_eq!(
            WindowSize::from_target_descriptor(&descriptor).unwrap(),
            (WindowSize::from_cells(90, 30, 10, 20), false)
        );
        descriptor[6].1 = Value::Bool(true);
        assert_eq!(
            WindowSize::from_target_descriptor(&descriptor).unwrap(),
            (WindowSize::from_cells(90, 30, 10, 20), true)
        );
    }

    #[test]
    fn a_marker_v2_or_truncated_target_descriptor_is_rejected() {
        let descriptor = |anchor_version: u64| {
            vec![
                (0, Value::Unsigned(900)),
                (1, Value::Unsigned(600)),
                (2, Value::Unsigned(90)),
                (3, Value::Unsigned(30)),
                (4, Value::Unsigned(10)),
                (5, Value::Unsigned(20)),
                (6, Value::Bool(true)),
                (7, Value::Unsigned(anchor_version)),
                (8, Value::Unsigned(64)),
            ]
        };
        assert_eq!(
            WindowSize::from_target_descriptor(&descriptor(2))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let mut truncated = descriptor(3);
        truncated.pop();
        assert_eq!(
            WindowSize::from_target_descriptor(&truncated)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_single_row_target_cannot_host_a_page_and_a_status_row() {
        let descriptor = vec![
            (0, Value::Unsigned(900)),
            (1, Value::Unsigned(20)),
            (2, Value::Unsigned(90)),
            (3, Value::Unsigned(1)),
            (4, Value::Unsigned(10)),
            (5, Value::Unsigned(20)),
            (6, Value::Bool(true)),
            (7, Value::Unsigned(3)),
            (8, Value::Unsigned(64)),
        ];
        assert_eq!(
            WindowSize::from_target_descriptor(&descriptor)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
