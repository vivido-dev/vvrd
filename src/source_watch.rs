//! Filesystem change detection for source documents that support automatic reload.

use std::{
    env,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context as _, ensure};
use flume::{Receiver, Sender};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

/// Quiet period after the last filesystem notification before reloading the source.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);
/// Bounds bursts while leaving room for a watcher error alongside coalescible change events.
const EVENT_CHANNEL_CAPACITY: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceWatchEvent {
    Changed,
    Error(String),
}

/// Holds the platform watcher alive while exposing its bounded event stream to the UI thread.
pub struct SourceWatcher {
    pub events: Receiver<SourceWatchEvent>,
    _watcher: RecommendedWatcher,
}

impl SourceWatcher {
    /// Watches the source path and its canonical target, if different, to support symlinked files
    /// and editor save strategies that atomically replace the source directory entry.
    pub fn new(source: &Path) -> anyhow::Result<Self> {
        let source = absolute_path(source).context("cannot resolve source path for hot reload")?;
        let mut targets = vec![source.clone()];
        if let Ok(canonical) = source.canonicalize()
            && canonical != source
        {
            targets.push(canonical);
        }
        targets.sort_unstable();
        targets.dedup();

        let (event_tx, events) = flume::bounded(EVENT_CHANNEL_CAPACITY);
        let callback_targets = targets.clone();
        let callback_tx = event_tx.clone();
        let mut watcher = notify::recommended_watcher(move |result| {
            handle_notify_result(result, &callback_targets, &callback_tx);
        })
        .context("cannot create source watcher")?;

        let mut parents = targets
            .iter()
            .filter_map(|target| target.parent().map(Path::to_path_buf))
            .collect::<Vec<_>>();
        parents.sort_unstable();
        parents.dedup();
        ensure!(!parents.is_empty(), "source path has no watchable parent");
        for parent in parents {
            watcher
                .watch(&parent, RecursiveMode::NonRecursive)
                .with_context(|| format!("cannot watch source directory {}", parent.display()))?;
        }

        Ok(Self {
            events,
            _watcher: watcher,
        })
    }
}

#[derive(Debug, Default)]
pub struct ReloadDebouncer {
    deadline: Option<Instant>,
}

impl ReloadDebouncer {
    pub fn note_change(&mut self, now: Instant) {
        self.deadline = Some(now + RELOAD_DEBOUNCE);
    }

    /// Returns true once a settled change is due and the source exists again. A removal clears the
    /// pending change; a later create notification schedules a fresh attempt.
    pub fn take_due(&mut self, now: Instant, source_exists: bool) -> bool {
        let Some(deadline) = self.deadline else {
            return false;
        };
        if now < deadline {
            return false;
        }
        self.deadline = None;
        source_exists
    }
}

pub fn supports_hot_reload(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("md" | "markdown" | "mkd" | "docx" | "pptx")
    )
}

fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir().map(|directory| directory.join(path))
    }
}

fn handle_notify_result(
    result: notify::Result<Event>,
    targets: &[PathBuf],
    events: &Sender<SourceWatchEvent>,
) {
    match result {
        Ok(event) if is_relevant_event(&event, targets) => {
            let _ = events.try_send(SourceWatchEvent::Changed);
        }
        Ok(_) => {}
        Err(error) => {
            let error = error.to_string();
            log::warn!("hot reload watcher error: {error}");
            let _ = events.try_send(SourceWatchEvent::Error(error));
        }
    }
}

fn is_relevant_event(event: &Event, targets: &[PathBuf]) -> bool {
    matches!(
        event.kind,
        EventKind::Any | EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) && event.paths.iter().any(|path| targets.contains(path))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use notify::event::{AccessKind, CreateKind, ModifyKind, RenameMode};

    use super::*;
    use crate::{
        geometry::WindowSize,
        markup::ThemeMode,
        renderer::{PaperStyle, RenderCmd, RenderEvent, RenderOptions, RenderThread},
    };

    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_markdown_path() -> PathBuf {
        env::current_dir().unwrap().join("target").join(format!(
            "vvrd-source-watch-{}-{}.md",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn automatic_reload_formats_are_explicit_and_case_insensitive() {
        for path in [
            "notes.md",
            "notes.MARKDOWN",
            "notes.MkD",
            "report.docx",
            "slides.PPTX",
        ] {
            assert!(supports_hot_reload(Path::new(path)), "{path}");
        }
        for path in [
            "diagram.mmd",
            "paper.pdf",
            "book.epub",
            "deck.odp",
            "text.odt",
        ] {
            assert!(!supports_hot_reload(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn relevant_events_match_only_mutations_of_the_source_path() {
        let source = PathBuf::from("/tmp/document.md");
        let targets = [source.clone()];
        let modify = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(source.clone());
        let replace = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/tmp/.document.md.tmp"))
            .add_path(source.clone());
        let create = Event::new(EventKind::Create(CreateKind::File)).add_path(source.clone());
        let unrelated =
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path(PathBuf::from("/tmp/other.md"));
        let access = Event::new(EventKind::Access(AccessKind::Any)).add_path(source);

        assert!(is_relevant_event(&modify, &targets));
        assert!(is_relevant_event(&replace, &targets));
        assert!(is_relevant_event(&create, &targets));
        assert!(!is_relevant_event(&unrelated, &targets));
        assert!(!is_relevant_event(&access, &targets));
    }

    #[test]
    fn debounce_coalesces_bursts_and_waits_for_recreation() {
        let start = Instant::now();
        let mut debounce = ReloadDebouncer::default();
        debounce.note_change(start);
        debounce.note_change(start + Duration::from_millis(200));
        assert!(!debounce.take_due(start + Duration::from_millis(400), true));
        assert!(debounce.take_due(start + Duration::from_millis(500), true));
        assert!(!debounce.take_due(start + Duration::from_millis(800), true));

        debounce.note_change(start + Duration::from_secs(1));
        assert!(!debounce.take_due(start + Duration::from_millis(1_300), false));
        debounce.note_change(start + Duration::from_millis(1_400));
        assert!(debounce.take_due(start + Duration::from_millis(1_700), true));
    }

    #[test]
    fn markdown_change_can_drive_transactional_reload_and_render_new_content() {
        let path = temp_markdown_path();
        fs::write(&path, "# Before\n").unwrap();
        let watcher = SourceWatcher::new(&path).unwrap();
        // FSEvents creates its stream asynchronously after `watch`; allow the platform stream to
        // arm before mutating the fixture. Production starts this watcher before renderer setup,
        // which provides the same ordering without an artificial delay.
        std::thread::sleep(Duration::from_millis(250));
        let viewport = WindowSize::from_cells(80, 24, 10, 20);
        let renderer = RenderThread::spawn(
            path.clone(),
            viewport,
            PaperStyle {
                theme: ThemeMode::Light,
                landscape: false,
            },
        );
        assert!(matches!(
            renderer
                .events
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            RenderEvent::Opened {
                document_revision: 1,
                ..
            }
        ));

        fs::write(&path, "# After\n\nchanged content\n").unwrap();
        assert!(matches!(
            watcher
                .events
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            SourceWatchEvent::Changed
        ));
        renderer.commands.send(RenderCmd::Reload).unwrap();
        assert!(matches!(
            renderer
                .events
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            RenderEvent::Opened {
                document_revision: 2,
                reloaded: true,
                ..
            }
        ));
        renderer
            .commands
            .send(RenderCmd::Render {
                page: 0,
                options: RenderOptions::for_viewport(viewport, 1),
            })
            .unwrap();
        assert!(matches!(
            renderer.events.recv_timeout(Duration::from_secs(10)).unwrap(),
            RenderEvent::Page { text, .. } if text.contains("changed content")
        ));

        renderer.shutdown();
        drop(watcher);
        fs::remove_file(path).unwrap();
    }
}
