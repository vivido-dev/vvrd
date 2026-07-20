use std::{
    collections::VecDeque,
    sync::Arc,
    thread::{self, JoinHandle},
};

use flume::{Receiver, Sender};
use vivid_sdk::{ProducerSession, SourceEvent};

use crate::{
    compositor::{PageImage, ViewTransform, compose_view},
    geometry::WindowSize,
    presenter::{Presenter, VividPresenter},
};

pub enum PresentCmd {
    ShowView {
        image: Arc<PageImage>,
        viewport: WindowSize,
        transform: ViewTransform,
    },
    SetVisible(bool),
    Resize(WindowSize),
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
    pub fn spawn(session: ProducerSession, viewport: WindowSize) -> Self {
        let (commands, command_rx) = flume::unbounded();
        let (event_tx, events) = flume::unbounded();
        let join = thread::Builder::new()
            .name("vvrd-vivid".to_owned())
            .spawn(move || run(session, viewport, command_rx, event_tx))
            .expect("failed to spawn Vivid presenter thread");
        Self {
            commands,
            events,
            join: Some(join),
        }
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
    session: ProducerSession,
    viewport: WindowSize,
    commands: Receiver<PresentCmd>,
    events: Sender<PresentEvent>,
) {
    let mut presenter = match VividPresenter::new(session, viewport) {
        Ok(presenter) => presenter,
        Err(error) => {
            let _ = events.send(PresentEvent::Error(error.to_string()));
            let _ = events.send(PresentEvent::Stopped);
            return;
        }
    };
    let _ = events.send(PresentEvent::Ready);

    let mut deferred = VecDeque::new();
    while let Some(command) = next_command(&commands, &mut deferred) {
        let result = match command {
            PresentCmd::ShowView {
                image,
                viewport,
                transform,
            } => compose_view((*image).clone(), viewport, transform)
                .map_err(|error| std::io::Error::other(error.to_string()))
                .and_then(|frame| {
                    presenter.show_frame(&frame.rgba).map(|frame_id| {
                        Some(PresentEvent::FrameShown {
                            frame_id,
                            content_width: frame.content_width,
                            content_height: frame.content_height,
                        })
                    })
                }),
            PresentCmd::SetVisible(visible) => presenter.set_visible(visible).map(|()| None),
            PresentCmd::Resize(viewport) => presenter.resize(viewport).map(|()| None),
            PresentCmd::Shutdown => break,
        };
        let mut source_interrupted = false;
        while let Some(event) = presenter.take_source_event() {
            match event {
                SourceEvent::Visibility(visible) => {
                    source_interrupted |= !visible;
                    let _ = events.send(PresentEvent::Visibility(visible));
                }
                SourceEvent::Lost(error) => {
                    source_interrupted = true;
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
        match result {
            Ok(Some(event)) => {
                let _ = events.send(event);
            }
            Ok(None) => {}
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

fn next_command(
    commands: &Receiver<PresentCmd>,
    deferred: &mut VecDeque<PresentCmd>,
) -> Option<PresentCmd> {
    let mut command = deferred.pop_front().or_else(|| commands.recv().ok())?;
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
    Some(command)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .send(PresentCmd::Resize(WindowSize::from_cells(80, 24, 10, 20)))
            .unwrap();
        sender.send(view(3)).unwrap();
        let mut deferred = VecDeque::new();
        assert!(
            matches!(next_command(&receiver, &mut deferred), Some(PresentCmd::ShowView { image, .. }) if image.pixels[0] == 2)
        );
        assert!(matches!(
            next_command(&receiver, &mut deferred),
            Some(PresentCmd::Resize(_))
        ));
        assert!(
            matches!(next_command(&receiver, &mut deferred), Some(PresentCmd::ShowView { image, .. }) if image.pixels[0] == 3)
        );
    }
}
