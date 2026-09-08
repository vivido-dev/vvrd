use std::io::{Write as _, stdout};

use anyhow::Context as _;
use crossterm::{
    cursor, execute,
    style::Print,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

use crate::geometry::WindowSize;
use crate::renderer::{LinkInfo, TocEntry};

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> anyhow::Result<Self> {
        execute!(
            stdout(),
            EnterAlternateScreen,
            cursor::Hide,
            crossterm::event::EnableMouseCapture
        )
        .context("failed to enter alternate screen")?;
        if let Err(error) = enable_raw_mode() {
            reset_terminal();
            return Err(error).context("failed to enable raw mode");
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        reset_terminal();
    }
}

pub fn reset_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        stdout(),
        LeaveAlternateScreen,
        cursor::Show,
        crossterm::event::DisableMouseCapture
    );
}

#[cfg(unix)]
pub fn suspend_and_resume() -> anyhow::Result<()> {
    reset_terminal();
    nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP).context("failed to suspend vvrd")?;
    let guard = TerminalGuard::enter()?;
    std::mem::forget(guard);
    Ok(())
}

pub fn clear_page_area(size: WindowSize) -> anyhow::Result<()> {
    let mut output = stdout().lock();
    for row in 0..size.page_rows() {
        execute!(
            output,
            cursor::MoveTo(0, row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    output.flush()?;
    Ok(())
}

pub fn draw_status(size: WindowSize, text: &str) -> anyhow::Result<()> {
    let width = size.cols as usize;
    let line = safe_line(text, width);
    execute!(
        stdout(),
        cursor::MoveTo(0, size.rows.saturating_sub(1)),
        Print(line)
    )?;
    Ok(())
}

pub fn draw_loading(size: WindowSize) -> anyhow::Result<()> {
    clear_page_area(size)?;
    let text = "Loading...";
    execute!(
        stdout(),
        cursor::MoveTo(
            size.cols.saturating_sub(text.len() as u16) / 2,
            size.page_rows() / 2
        ),
        Print(text)
    )?;
    Ok(())
}

pub fn draw_toc(
    size: WindowSize,
    toc: &[TocEntry],
    selected: usize,
    current_page: usize,
) -> anyhow::Result<()> {
    clear_page_area(size)?;
    let rows = size.page_rows() as usize;
    let start = selected.saturating_sub(rows.saturating_sub(1));
    for (row, (index, entry)) in toc.iter().enumerate().skip(start).take(rows).enumerate() {
        let marker = if index == selected {
            ">"
        } else if entry.page == current_page {
            "*"
        } else {
            " "
        };
        let line = format!(
            "{marker}{}{}  (p.{})",
            "  ".repeat(entry.level.min(size.cols as usize / 2)),
            entry.title,
            entry.page.saturating_add(1)
        );
        draw_line(size, row as u16, &line)?;
    }
    Ok(())
}

pub fn draw_metadata(size: WindowSize, metadata: &[(String, String)]) -> anyhow::Result<()> {
    clear_page_area(size)?;
    draw_line(size, 0, "Document Metadata (Esc to close)")?;
    for (row, (key, value)) in metadata
        .iter()
        .take(size.page_rows().saturating_sub(2) as usize)
        .enumerate()
    {
        draw_line(size, row as u16 + 2, &format!("{key}: {value}"))?;
    }
    Ok(())
}

pub fn draw_links(
    size: WindowSize,
    links: &[LinkInfo],
    input: &str,
    page: usize,
) -> anyhow::Result<()> {
    clear_page_area(size)?;
    draw_line(
        size,
        0,
        &format!("Links on page {} (Esc to close)", page + 1),
    )?;
    for (row, link) in links
        .iter()
        .take(size.page_rows().saturating_sub(3) as usize)
        .enumerate()
    {
        draw_line(size, row as u16 + 2, &format!("{}: {}", row + 1, link.text))?;
    }
    draw_line(
        size,
        size.page_rows().saturating_sub(1),
        &format!("Enter number to follow: {input}"),
    )
}

pub fn draw_help(size: WindowSize) -> anyhow::Result<()> {
    clear_page_area(size)?;
    let lines = [
        "vvrd keys (Esc or ? to close)",
        "←/→, h/l   previous/next page (pan with h/l in zoom)",
        "↑/↓        scroll; j/k always turn pages",
        "PgUp/PgDn viewport/page; Space next page; g go to page",
        "z zoom mode; o/O zoom in/out; r rotate; i invert",
        "c auto-crop; d warm tint; </> EPUB font size",
        "/ search; n/N next/previous result; t table of contents",
        "M metadata; f links; e export PNG; R/F5 manual reload; q quit",
    ];
    for (row, line) in lines
        .into_iter()
        .take(size.page_rows() as usize)
        .enumerate()
    {
        draw_line(size, row as u16, line)?;
    }
    Ok(())
}

fn draw_line(size: WindowSize, row: u16, text: &str) -> anyhow::Result<()> {
    let display = safe_line(text, size.cols as usize);
    execute!(stdout(), cursor::MoveTo(0, row), Print(display))?;
    Ok(())
}

/// Only printable graphemes enter Print; clipping never splits a wide cluster.
fn safe_line(text: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut result = String::new();
    let mut used = 0;
    for cluster in text.graphemes(true) {
        let safe = if cluster.chars().any(|ch| ch.is_control()) {
            "�"
        } else {
            cluster
        };
        let cells = UnicodeWidthStr::width(safe);
        if cells > width.saturating_sub(used) {
            break;
        }
        // Bound zero-width sequences too; they must not turn a short status row into huge output.
        if result.len().saturating_add(safe.len()) > width.saturating_mul(32) {
            break;
        }
        result.push_str(safe);
        used += cells;
    }
    result.push_str(&" ".repeat(width.saturating_sub(used)));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_text_cannot_execute_controls_or_split_graphemes() {
        assert!(
            !safe_line("\x1b]52;c;evil\x07\r\n", 40)
                .chars()
                .any(char::is_control)
        );
        assert_eq!(safe_line("中a", 2), "中");
        assert_eq!(safe_line("e\u{301}中", 2), "e\u{301} ");
        assert_eq!(safe_line("👩‍💻x", 2), "👩‍💻");
    }
}
