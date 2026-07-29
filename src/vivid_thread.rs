use std::{
    collections::VecDeque,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use flume::{Receiver, RecvTimeoutError, Sender};
use vivid_sdk::{ProducerSession, SourceDescriptor, SourceEvent};

use crate::{
    compositor::{ComposedFrame, PageImage, ViewTransform, compose_view, plan_frame_delta},
    geometry::WindowSize,
    presenter::{Presenter, VividPresenter},
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
    Visibility(bool),
    DisplayChanged {
        viewport: WindowSize,
        settled: bool,
    },
    SourceLost(String),
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
        session: ProducerSession,
        viewport: WindowSize,
        capture_policy: u64,
        descriptor: SourceDescriptor,
        initial_page: usize,
    ) -> std::io::Result<Self> {
        let presenter =
            VividPresenter::new(session, viewport, capture_policy, descriptor, initial_page)?;
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
    let mut display_state = presenter.display_state();
    let mut previous = None;
    loop {
        let (_, resend_full) = service_source_events(&mut presenter, &events);
        if resend_full && let Some(previous) = &previous {
            resend_full_frame(&mut presenter, previous, &events);
        }
        let display = presenter.display_state();
        if display != display_state {
            display_state = display;
            match WindowSize::current(display)
                .map_err(|error| std::io::Error::other(error.to_string()))
                .and_then(|viewport| {
                    presenter.resize(viewport, display.settled)?;
                    Ok(viewport)
                }) {
                Ok(viewport) => {
                    let _ = events.send(PresentEvent::DisplayChanged {
                        viewport,
                        settled: display.settled,
                    });
                }
                Err(error) => {
                    let _ = events.send(PresentEvent::Error(error.to_string()));
                }
            }
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
            PresentCmd::ShowView { viewport, .. } if viewport != presenter.source_viewport() => {
                log::debug!(
                    "dropped view composed for {}x{} cells; source viewport is {}x{} cells",
                    viewport.cols,
                    viewport.rows,
                    presenter.source_viewport().cols,
                    presenter.source_viewport().rows
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
        let (source_interrupted, resend_full) = service_source_events(&mut presenter, &events);
        if resend_full && let Some(previous) = &previous {
            resend_full_frame(&mut presenter, previous, &events);
        }
        match result {
            Ok(Some(event)) if !source_interrupted => {
                let _ = events.send(event);
            }
            Ok(_) => {}
            Err(error) if !source_interrupted => {
                let _ = events.send(PresentEvent::Error(error.to_string()));
            }
            Err(_) => {}
        }
    }

    if let Err(error) = presenter.teardown() {
        let _ = events.send(PresentEvent::Error(error.to_string()));
    }
    let _ = events.send(PresentEvent::Stopped);
}

fn service_source_events(
    presenter: &mut VividPresenter,
    events: &Sender<PresentEvent>,
) -> (bool, bool) {
    let mut interrupted = false;
    let mut resend_full = false;
    if let Some(reason) = presenter.take_full_frame_request() {
        presenter.require_full_frame(reason);
        interrupted = true;
        resend_full = true;
    }
    while let Some(event) = presenter.take_source_event() {
        match event {
            SourceEvent::Visibility(visible) => {
                interrupted |= !visible;
                let _ = events.send(PresentEvent::Visibility(visible));
            }
            SourceEvent::Lost(error) => {
                interrupted = true;
                resend_full = true;
                match presenter.recover_source() {
                    Ok(()) => {
                        let _ = events.send(PresentEvent::SourceLost(error));
                    }
                    Err(recovery_error) => {
                        let _ = events.send(PresentEvent::Error(format!(
                            "{error}; source recovery failed: {recovery_error}"
                        )));
                    }
                }
            }
            SourceEvent::NeedKeyframe(_) => {}
        }
    }
    (interrupted, resend_full)
}

fn resend_full_frame(
    presenter: &mut VividPresenter,
    previous: &PreviousComposition,
    events: &Sender<PresentEvent>,
) {
    if previous.viewport != presenter.source_viewport() {
        // The retained composition predates the current source geometry, so it cannot serve as this
        // source's keyframe. The full-frame request stays armed, which makes the re-render that
        // follows the resize the keyframe instead.
        log::debug!(
            "skipped full-frame recovery: retained composition is {}x{} cells, source viewport is {}x{} cells",
            previous.viewport.cols,
            previous.viewport.rows,
            presenter.source_viewport().cols,
            presenter.source_viewport().rows
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

    use vivid_protocol::messages;
    use vivid_sdk::ProducerConfig;

    fn dry_run_session() -> ProducerSession {
        ProducerSession::connect(&ProducerConfig {
            endpoint: None,
            bulk_endpoint: None,
            token: None,
            dry_run: true,
            trace_dir: None,
            verbose: false,
            producer: "vvrd-test".to_owned(),
            producer_version: "1".to_owned(),
            required_features: vec![
                messages::FEATURE_RASTER_RGBA8,
                messages::FEATURE_SCENE_TRANSACTIONS,
                messages::FEATURE_GRID_CELL_NODES,
                messages::FEATURE_CREDIT_FLOW_CONTROL,
            ],
            optional_features: vec![
                messages::FEATURE_SOURCE_DESCRIPTOR_V1,
                messages::FEATURE_RASTER_DELTA_V1,
            ],
            authentication_kind: messages::AUTHENTICATION_WINDOW_ROOT,
            allow_version_retry: false,
        })
        .unwrap()
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

    fn test_descriptor() -> SourceDescriptor {
        SourceDescriptor {
            role: messages::SOURCE_ROLE_DOCUMENT,
            title: "vvrd-test".to_owned(),
            content_revision: 1,
            semantic_availability: 0,
            locator: String::new(),
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
