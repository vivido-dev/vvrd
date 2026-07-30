use std::{
    collections::VecDeque,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use flume::{Receiver, RecvTimeoutError, Sender};
use vivid_protocol::registry;
use vivid_sdk::{Session, SurfaceDescriptor};

use crate::{
    compositor::{ComposedFrame, PageImage, ViewTransform, compose_view, plan_frame_delta},
    geometry::{TargetViewport, WindowSize},
    presenter::{Presenter, PresenterSignal, VividPresenter},
};

struct PreviousComposition {
    image: Arc<PageImage>,
    viewport: WindowSize,
    transform: ViewTransform,
    frame: ComposedFrame,
}

pub enum PresentCmd {
    ShowView {
        image: Arc<PageImage>,
        viewport: WindowSize,
        transform: ViewTransform,
    },
    SetVisible(bool),
    Resize {
        viewport: WindowSize,
        settled: bool,
    },
    UpdateContent {
        page: usize,
        search_term: Option<String>,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum PresentEvent {
    Ready,
    FrameShown {
        frame_id: u64,
        content_width: u32,
        content_height: u32,
    },
    TargetChanged {
        viewport: WindowSize,
        settled: bool,
    },
    /// The terminal is too small to host a page and its status row. Nothing can be presented until
    /// it grows, but the session and every object in it stay valid.
    TargetTooSmall {
        cols: u16,
        rows: u16,
    },
    /// The raster track was lost and replaced. The document surface and its placement survived.
    TrackLost(String),
    Error(String),
    Stopped,
}

pub struct VividThread {
    pub commands: Sender<PresentCmd>,
    pub events: Receiver<PresentEvent>,
    join: Option<JoinHandle<()>>,
}

impl VividThread {
    pub fn spawn(
        session: Session,
        viewport: WindowSize,
        policy: u64,
        descriptor: SurfaceDescriptor,
        initial_page: usize,
    ) -> std::io::Result<Self> {
        let presenter = VividPresenter::new(session, viewport, policy, descriptor, initial_page)?;
        let (commands, command_rx) = flume::bounded(presenter.command_queue_capacity());
        let (event_tx, events) = flume::unbounded();
        let join = thread::Builder::new()
            .name("vvrd-vivid".to_owned())
            .spawn(move || run(presenter, command_rx, event_tx))?;
        Ok(Self {
            commands,
            events,
            join: Some(join),
        })
    }

    pub fn shutdown(mut self) {
        let _ = self.commands.send(PresentCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for VividThread {
    fn drop(&mut self) {
        let _ = self.commands.send(PresentCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(
    mut presenter: VividPresenter,
    commands: Receiver<PresentCmd>,
    events: Sender<PresentEvent>,
) {
    let _ = events.send(PresentEvent::Ready);

    let mut deferred = VecDeque::new();
    let mut previous = None;
    loop {
        let signals = service_presenter_signals(&mut presenter, &events);
        if signals.stop {
            break;
        }
        if signals.resend_full
            && let Some(previous) = &previous
        {
            resend_full_frame(&mut presenter, previous, &events);
        }
        if signals.target_changed {
            apply_target_change(&mut presenter, &events);
        }

        let command = match next_command(&commands, &mut deferred) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let result = match command {
            // The reader composes against the viewport it knew when the view was queued, while a
            // settled display change replaces the raster source here. A detach/reattach that lands
            // between the two (new grid or cell size) leaves already-queued views sized for the old
            // source; submitting one would fail `show_frame`'s length check and take the reader
            // down. Drop it instead: the resize already published `DisplayChanged`, and the
            // re-render it triggers arrives at the current geometry.
            PresentCmd::ShowView { viewport, .. } if viewport != presenter.track_viewport() => {
                log::debug!(
                    "dropped view composed for {}x{} cells; track viewport is {}x{} cells",
                    viewport.cols,
                    viewport.rows,
                    presenter.track_viewport().cols,
                    presenter.track_viewport().rows
                );
                Ok(None)
            }
            PresentCmd::ShowView {
                image,
                viewport,
                transform,
            } => compose_view((*image).clone(), viewport, transform)
                .map_err(|error| std::io::Error::other(error.to_string()))
                .and_then(|frame| {
                    let delta = if let (Some(previous), Some(limit)) =
                        (previous.as_ref(), presenter.delta_operation_limit())
                        && previous.viewport == viewport
                    {
                        plan_frame_delta(
                            &previous.frame,
                            &frame,
                            viewport,
                            previous.transform,
                            transform,
                            Arc::ptr_eq(&previous.image, &image),
                            limit,
                        )
                        .map_err(|error| std::io::Error::other(error.to_string()))?
                    } else {
                        None
                    };
                    let frame_id = presenter.show_frame(&frame.rgba, delta.as_ref())?;
                    let event = PresentEvent::FrameShown {
                        frame_id,
                        content_width: frame.content_width,
                        content_height: frame.content_height,
                    };
                    previous = Some(PreviousComposition {
                        image,
                        viewport,
                        transform,
                        frame,
                    });
                    Ok(Some(event))
                }),
            PresentCmd::SetVisible(visible) => presenter.set_visible(visible).map(|()| None),
            PresentCmd::Resize { viewport, settled } => {
                presenter.resize(viewport, settled).map(|()| None)
            }
            PresentCmd::UpdateContent { page, search_term } => presenter
                .update_content_descriptor(page, search_term)
                .map(|()| None),
            PresentCmd::Shutdown => break,
        };
        let signals = service_presenter_signals(&mut presenter, &events);
        if signals.resend_full
            && let Some(previous) = &previous
        {
            resend_full_frame(&mut presenter, previous, &events);
        }
        if signals.target_changed {
            apply_target_change(&mut presenter, &events);
        }
        match result {
            Ok(Some(event)) if !signals.interrupted => {
                let _ = events.send(event);
            }
            Ok(_) => {}
            Err(error) if !signals.interrupted => {
                let _ = events.send(PresentEvent::Error(error.to_string()));
            }
            Err(_) => {}
        }
        if signals.stop {
            break;
        }
    }

    if let Err(error) = presenter.teardown() {
        let _ = events.send(PresentEvent::Error(error.to_string()));
    }
    let _ = events.send(PresentEvent::Stopped);
}

#[derive(Debug, Default, Clone, Copy)]
struct SignalOutcome {
    /// A recovery displaced the in-flight command, so its result must not be reported.
    interrupted: bool,
    /// The retained composition has to be resubmitted as this generation's full frame.
    resend_full: bool,
    target_changed: bool,
    stop: bool,
}

/// Drain presenter traffic and apply the recovery each signal demands.
///
/// The three recoveries are deliberately distinct: a full-frame request keeps the channel, a
/// channel loss keeps the track, and a track loss keeps the surface, its scene node, and the
/// document's semantic identity.
fn service_presenter_signals(
    presenter: &mut VividPresenter,
    events: &Sender<PresentEvent>,
) -> SignalOutcome {
    let mut outcome = SignalOutcome::default();
    while let Some(signal) = presenter.take_signal() {
        match signal {
            PresenterSignal::FullFrameNeeded(reason) => {
                presenter.require_full_frame(reason);
                outcome.interrupted = true;
                outcome.resend_full = true;
            }
            PresenterSignal::ChannelLost(diagnostic) => {
                outcome.interrupted = true;
                outcome.resend_full = true;
                if let Err(error) = presenter.recover_channel(registry::error::DEVICE_LOST) {
                    let _ = events.send(PresentEvent::Error(format!(
                        "{diagnostic}; channel recovery failed: {error}"
                    )));
                    outcome.stop = true;
                }
            }
            PresenterSignal::TrackLost(diagnostic) => {
                outcome.interrupted = true;
                outcome.resend_full = true;
                match presenter.recover_track() {
                    Ok(()) => {
                        let _ = events.send(PresentEvent::TrackLost(diagnostic));
                    }
                    Err(error) => {
                        let _ = events.send(PresentEvent::Error(format!(
                            "{diagnostic}; track recovery failed: {error}"
                        )));
                        outcome.stop = true;
                    }
                }
            }
            PresenterSignal::TargetChanged => outcome.target_changed = true,
            PresenterSignal::ConnectionClosed(diagnostic) => {
                let _ = events.send(PresentEvent::Error(diagnostic));
                outcome.interrupted = true;
                outcome.stop = true;
            }
        }
    }
    outcome
}

fn apply_target_change(presenter: &mut VividPresenter, events: &Sender<PresentEvent>) {
    let (target, settled) = match presenter.target_viewport() {
        Ok(target) => target,
        Err(error) => {
            let _ = events.send(PresentEvent::Error(error.to_string()));
            return;
        }
    };
    // A terminal with no room for a page and its status row is a target to wait out, not a failed
    // session: the surface, track, and node are all still valid, and a window shrunk that far is
    // usually on its way back. Resizing into it would rebuild the track around geometry nothing
    // can be drawn into.
    let viewport = match target {
        TargetViewport::Presentable(viewport) => viewport,
        TargetViewport::TooSmall { cols, rows } => {
            let _ = events.send(PresentEvent::TargetTooSmall { cols, rows });
            return;
        }
    };
    match presenter.resize(viewport, settled) {
        Ok(()) => {
            let _ = events.send(PresentEvent::TargetChanged { viewport, settled });
        }
        Err(error) => {
            let _ = events.send(PresentEvent::Error(error.to_string()));
        }
    }
}

fn resend_full_frame(
    presenter: &mut VividPresenter,
    previous: &PreviousComposition,
    events: &Sender<PresentEvent>,
) {
    if previous.viewport != presenter.track_viewport() {
        // The retained composition predates the current track geometry, so it cannot serve as this
        // generation's full frame. The full-frame request stays armed, which makes the re-render
        // that follows the resize the recovery unit instead.
        log::debug!(
            "skipped full-frame recovery: retained composition is {}x{} cells, track viewport is {}x{} cells",
            previous.viewport.cols,
            previous.viewport.rows,
            presenter.track_viewport().cols,
            presenter.track_viewport().rows
        );
        return;
    }
    match presenter.show_frame(&previous.frame.rgba, None) {
        Ok(frame_id) => {
            let _ = events.send(PresentEvent::FrameShown {
                frame_id,
                content_width: previous.frame.content_width,
                content_height: previous.frame.content_height,
            });
        }
        Err(error) => {
            let _ = events.send(PresentEvent::Error(format!(
                "full-frame recovery failed: {error}"
            )));
        }
    }
}

fn next_command(
    commands: &Receiver<PresentCmd>,
    deferred: &mut VecDeque<PresentCmd>,
) -> Result<PresentCmd, RecvTimeoutError> {
    let mut command = match deferred.pop_front() {
        Some(command) => command,
        None => commands.recv_timeout(Duration::from_millis(20))?,
    };
    if matches!(command, PresentCmd::ShowView { .. }) {
        while let Ok(next) = commands.try_recv() {
            match next {
                PresentCmd::ShowView { .. } => command = next,
                other => {
                    deferred.push_back(other);
                    break;
                }
            }
        }
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    use vivid_sdk::{ProducerConfig, SurfaceRole};

    fn dry_run_session() -> Session {
        Session::connect(ProducerConfig::offline()).unwrap()
    }

    fn test_page() -> Arc<PageImage> {
        Arc::new(PageImage {
            pixels: vec![255; 3 * 4 * 4],
            width: 4,
            height: 4,
            row_stride: 3 * 4,
            highlights: Vec::new(),
        })
    }

    #[test]
    fn consecutive_frames_coalesce_without_crossing_control_commands() {
        let (sender, receiver) = flume::unbounded();
        let viewport = WindowSize::from_cells(1, 2, 1, 1);
        let view = |value| PresentCmd::ShowView {
            image: Arc::new(PageImage {
                pixels: vec![value, 0, 0],
                width: 1,
                height: 1,
                row_stride: 3,
                highlights: Vec::new(),
            }),
            viewport,
            transform: ViewTransform::default(),
        };
        sender.send(view(1)).unwrap();
        sender.send(view(2)).unwrap();
        sender
            .send(PresentCmd::Resize {
                viewport: WindowSize::from_cells(80, 24, 10, 20),
                settled: true,
            })
            .unwrap();
        sender.send(view(3)).unwrap();
        let mut deferred = VecDeque::new();
        assert!(
            matches!(next_command(&receiver, &mut deferred), Ok(PresentCmd::ShowView { image, .. }) if image.pixels[0] == 2)
        );
        assert!(matches!(
            next_command(&receiver, &mut deferred),
            Ok(PresentCmd::Resize { settled: true, .. })
        ));
        assert!(
            matches!(next_command(&receiver, &mut deferred), Ok(PresentCmd::ShowView { image, .. }) if image.pixels[0] == 3)
        );
    }

    #[test]
    fn a_view_composed_before_a_settled_resize_is_dropped_instead_of_stopping_the_reader() {
        let before = WindowSize::from_cells(20, 6, 4, 8);
        let after = WindowSize::from_cells(40, 12, 5, 9);
        let vivid = VividThread::spawn(dry_run_session(), before, 0, test_descriptor(), 0).unwrap();
        assert!(matches!(
            vivid.events.recv_timeout(Duration::from_secs(5)),
            Ok(PresentEvent::Ready)
        ));
        vivid.commands.send(show_view(before)).unwrap();
        assert!(matches!(
            vivid.events.recv_timeout(Duration::from_secs(5)),
            Ok(PresentEvent::FrameShown { .. })
        ));

        // A reattach that changes the pane grid or cell size resizes the source here while the
        // reader still holds the old viewport, so its next view arrives sized for the old source.
        vivid
            .commands
            .send(PresentCmd::Resize {
                viewport: after,
                settled: true,
            })
            .unwrap();
        vivid.commands.send(show_view(before)).unwrap();
        assert!(matches!(
            vivid.events.recv_timeout(Duration::from_millis(250)),
            Err(RecvTimeoutError::Timeout)
        ));

        vivid.commands.send(show_view(after)).unwrap();
        assert!(matches!(
            vivid.events.recv_timeout(Duration::from_secs(5)),
            Ok(PresentEvent::FrameShown { .. })
        ));
        vivid.shutdown();
    }

    #[test]
    fn full_frame_recovery_skips_a_composition_from_the_previous_source_geometry() {
        let before = WindowSize::from_cells(20, 6, 4, 8);
        let after = WindowSize::from_cells(40, 12, 5, 9);
        let mut presenter =
            VividPresenter::new(dry_run_session(), before, 0, test_descriptor(), 0).unwrap();
        let image = test_page();
        let previous = PreviousComposition {
            frame: compose_view((*image).clone(), before, ViewTransform::default()).unwrap(),
            image,
            viewport: before,
            transform: ViewTransform::default(),
        };
        presenter.resize(after, true).unwrap();

        let (events, observed) = flume::unbounded();
        resend_full_frame(&mut presenter, &previous, &events);
        assert!(observed.try_recv().is_err());
    }

    fn test_descriptor() -> SurfaceDescriptor {
        SurfaceDescriptor {
            role: SurfaceRole::Document,
            title: "vvrd-test".to_owned(),
            semantic_content_revision: 1,
            semantic_availability: 0,
            locator_hint: String::new(),
        }
    }

    fn show_view(viewport: WindowSize) -> PresentCmd {
        PresentCmd::ShowView {
            image: test_page(),
            viewport,
            transform: ViewTransform::default(),
        }
    }
}
