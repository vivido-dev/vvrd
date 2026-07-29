use std::io;

use vivid_protocol::{
    media::{self, RasterDeltaOperation},
    messages::{self, SceneNodeConfig},
    wire::ConnectionKind,
};
use vivid_sdk::{
    DisplayState, MediaQueueLimits, MediaSender, ProducerSession, RasterSendKind, SourceDescriptor,
    SourceEvent, SourceHandle,
};

use crate::{
    compositor::{DeltaOperation, FrameDelta},
    geometry::WindowSize,
};

pub trait Presenter {
    fn show_frame(&mut self, rgba: &[u8], delta: Option<&FrameDelta>) -> io::Result<u64>;
    fn set_visible(&mut self, visible: bool) -> io::Result<()>;
    fn resize(&mut self, viewport: WindowSize, settled: bool) -> io::Result<()>;
    fn recover_source(&mut self) -> io::Result<()>;
    fn require_full_frame(&mut self, reason: u64);
    fn take_full_frame_request(&mut self) -> Option<u64>;
    fn take_source_event(&mut self) -> Option<SourceEvent>;
    fn teardown(&mut self) -> io::Result<()>;
}

const REQUESTED_DELTA_OPERATION_LIMIT: u32 = media::RASTER_DELTA_OPERATION_LIMIT;
const ACCUMULATED_DAMAGE_DENOMINATOR: u64 = 2;

pub struct VividPresenter {
    session: ProducerSession,
    sender: Option<MediaSender>,
    source_id: u64,
    node_id: u64,
    source_viewport: WindowSize,
    node_viewport: WindowSize,
    visible: bool,
    epoch: u32,
    frame_id: u64,
    force_full_frame: bool,
    accumulated_damage_pixels: u64,
    recovery_reason: Option<u64>,
    torn_down: bool,
    capture_policy: u64,
    descriptor: SourceDescriptor,
    semantic_state: (usize, Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeAction {
    None,
    UpdateNode,
    ReplaceSource,
}

fn resize_action(
    source_viewport: WindowSize,
    node_viewport: WindowSize,
    viewport: WindowSize,
    settled: bool,
) -> ResizeAction {
    if settled {
        if viewport == source_viewport && viewport == node_viewport {
            ResizeAction::None
        } else {
            ResizeAction::ReplaceSource
        }
    } else if viewport == node_viewport {
        ResizeAction::None
    } else {
        ResizeAction::UpdateNode
    }
}

impl VividPresenter {
    pub fn new(
        mut session: ProducerSession,
        viewport: WindowSize,
        capture_policy: u64,
        descriptor: SourceDescriptor,
        initial_page: usize,
    ) -> io::Result<Self> {
        let source_id = session.allocate_id()?;
        let node_id = session.allocate_id()?;
        let source = create_viewport_source(
            &mut session,
            source_id,
            viewport,
            capture_policy,
            &descriptor,
        )?;
        let sender = session.open_media_sender(source, ConnectionKind::Raster)?;
        session.create_scene_node(&scene_config(
            session.root_context_id(),
            node_id,
            source_id,
            viewport,
            true,
        ))?;

        Ok(Self {
            session,
            sender: Some(sender),
            source_id,
            node_id,
            source_viewport: viewport,
            node_viewport: viewport,
            visible: true,
            epoch: 1,
            frame_id: 0,
            force_full_frame: true,
            accumulated_damage_pixels: 0,
            recovery_reason: None,
            torn_down: false,
            capture_policy,
            descriptor,
            semantic_state: (initial_page, None),
        })
    }

    fn replace_source(
        &mut self,
        source_viewport: WindowSize,
        node_viewport: WindowSize,
    ) -> io::Result<()> {
        let old_source_id = self.source_id;
        let source_id = self.session.allocate_id()?;
        let source = create_viewport_source(
            &mut self.session,
            source_id,
            source_viewport,
            self.capture_policy,
            &self.descriptor,
        )?;
        let sender = match self
            .session
            .open_media_sender(source, ConnectionKind::Raster)
        {
            Ok(sender) => sender,
            Err(error) => {
                let _ = self.session.destroy_source(source_id);
                return Err(error);
            }
        };

        if let Err(error) = self.session.update_scene_node(&scene_config(
            self.session.root_context_id(),
            self.node_id,
            source_id,
            node_viewport,
            self.visible,
        )) {
            let _ = self.session.destroy_source(source_id);
            return Err(error);
        }

        let old_sender = self.sender.replace(sender);
        self.source_id = source_id;
        self.source_viewport = source_viewport;
        self.node_viewport = node_viewport;
        self.epoch = self.epoch.checked_add(1).unwrap_or(1);
        self.frame_id = 0;
        self.force_full_frame = true;
        self.accumulated_damage_pixels = 0;
        self.recovery_reason = None;
        // A media EOF before DESTROY_SOURCE is a source loss. Keep the old transport alive until
        // the presenter acknowledges the ordered source destruction; otherwise vvmux removes the
        // source on EOF and rejects the subsequent destroy because the source no longer exists.
        let result = self.session.destroy_source(old_source_id);
        drop(old_sender);
        result
    }

    pub fn delta_operation_limit(&self) -> Option<u32> {
        self.sender
            .as_ref()
            .and_then(|sender| sender.source().delta_operation_limit())
    }

    fn observe_recovery(&mut self, stage: &str, reason: u64) {
        if !self
            .session
            .supports(messages::FEATURE_OBSERVABILITY_CORE_V1)
        {
            return;
        }
        match self.session.query_source(self.source_id) {
            Ok(status) => log::debug!(
                "raster full-frame recovery {stage}: reason={reason} source={} epoch={} media_id={} media_sequence={}",
                self.source_id,
                status.epoch,
                status.last_media_id,
                status.last_media_sequence
            ),
            Err(error) => log::debug!(
                "raster full-frame recovery {stage}: reason={reason} observation unavailable: {error}"
            ),
        }
    }

    fn expected_frame_len(&self) -> io::Result<usize> {
        self.source_viewport
            .framebuffer_len()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
    }

    pub fn display_state(&self) -> DisplayState {
        self.session.display_state()
    }

    /// Viewport the live raster source was created for.
    ///
    /// Compositions are planned against a viewport captured before they were queued, so callers
    /// must compare it against this before submitting: a settled resize replaces the source, and
    /// `show_frame` rejects any frame that no longer matches it.
    pub fn source_viewport(&self) -> WindowSize {
        self.source_viewport
    }

    pub fn command_queue_capacity(&self) -> usize {
        let limits = self
            .sender
            .as_ref()
            .map(|sender| sender.source().media_queue_limits())
            .unwrap_or(MediaQueueLimits {
                max_bytes: 1,
                max_packets: 1,
            });
        command_queue_capacity(limits, self.expected_frame_len().unwrap_or(usize::MAX))
    }

    pub fn update_content_descriptor(
        &mut self,
        page: usize,
        search_term: Option<String>,
    ) -> io::Result<()> {
        if self.semantic_state == (page, search_term.clone()) {
            return Ok(());
        }
        self.descriptor.content_revision = self
            .descriptor
            .content_revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("document content revision space exhausted"))?;
        self.session
            .update_source_descriptor(self.source_id, &self.descriptor)?;
        self.semantic_state = (page, search_term);
        Ok(())
    }
}

fn command_queue_capacity(limits: MediaQueueLimits, frame_bytes: usize) -> usize {
    limits
        .max_packets
        .min(limits.max_bytes / frame_bytes.max(1))
        .max(1)
}

impl Presenter for VividPresenter {
    fn show_frame(&mut self, rgba: &[u8], delta: Option<&FrameDelta>) -> io::Result<u64> {
        if rgba.len() != self.expected_frame_len()? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "viewport frame has {} bytes, expected {}",
                    rgba.len(),
                    self.expected_frame_len()?
                ),
            ));
        }
        self.frame_id = self
            .frame_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("raster frame ID space exhausted"))?;
        let sender = self
            .sender
            .as_mut()
            .ok_or_else(|| io::Error::other("raster source is unavailable"))?;
        self.session.wait_until_visible(sender.source_mut())?;
        let width = self.source_viewport.page_area_width_px();
        let height = self.source_viewport.page_area_height_px();
        let frame_pixels = u64::from(width) * u64::from(height);
        let delta_allowed = delta.is_some_and(|delta| {
            !self.force_full_frame
                && !accumulated_damage_exceeds_fraction(
                    self.accumulated_damage_pixels,
                    delta.damaged_pixels,
                    frame_pixels,
                )
        });
        let kind = if delta_allowed {
            let delta = delta.expect("delta_allowed requires a delta");
            let operations: Vec<_> = delta
                .operations
                .iter()
                .map(|operation| match operation {
                    DeltaOperation::Copy {
                        destination_x,
                        destination_y,
                        width,
                        height,
                        source_x,
                        source_y,
                    } => RasterDeltaOperation::Copy {
                        destination_x: *destination_x,
                        destination_y: *destination_y,
                        width: *width,
                        height: *height,
                        source_x: *source_x,
                        source_y: *source_y,
                    },
                    DeltaOperation::Overwrite { rect, rgba } => RasterDeltaOperation::Overwrite {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                        rgba,
                    },
                })
                .collect();
            sender.send_raster_delta_or_full(
                self.epoch,
                self.frame_id,
                self.frame_id - 1,
                width,
                height,
                rgba,
                &operations,
            )?
        } else {
            sender.send_raster(self.epoch, self.frame_id, width, height, rgba)?;
            RasterSendKind::Full
        };
        match kind {
            RasterSendKind::Full => {
                self.force_full_frame = false;
                self.accumulated_damage_pixels = 0;
                if let Some(reason) = self.recovery_reason.take() {
                    self.observe_recovery("completed", reason);
                }
            }
            RasterSendKind::Delta => {
                self.accumulated_damage_pixels = self
                    .accumulated_damage_pixels
                    .saturating_add(delta.expect("delta send requires a plan").damaged_pixels);
            }
        }
        log::debug!(
            "submitted raster frame epoch={} frame_id={} kind={kind:?} damage_pixels={} accumulated_damage_pixels={}",
            self.epoch,
            self.frame_id,
            if kind == RasterSendKind::Full {
                frame_pixels
            } else {
                delta.expect("delta send requires a plan").damaged_pixels
            },
            self.accumulated_damage_pixels
        );
        Ok(self.frame_id)
    }

    fn set_visible(&mut self, visible: bool) -> io::Result<()> {
        if visible == self.visible {
            return Ok(());
        }
        self.session.update_scene_node(&scene_config(
            self.session.root_context_id(),
            self.node_id,
            self.source_id,
            self.node_viewport,
            visible,
        ))?;
        self.visible = visible;
        Ok(())
    }

    fn resize(&mut self, viewport: WindowSize, settled: bool) -> io::Result<()> {
        match resize_action(self.source_viewport, self.node_viewport, viewport, settled) {
            ResizeAction::None => Ok(()),
            ResizeAction::UpdateNode => {
                self.session.update_scene_node(&scene_config(
                    self.session.root_context_id(),
                    self.node_id,
                    self.source_id,
                    viewport,
                    self.visible,
                ))?;
                self.node_viewport = viewport;
                Ok(())
            }
            ResizeAction::ReplaceSource => self.replace_source(viewport, viewport),
        }
    }

    fn recover_source(&mut self) -> io::Result<()> {
        self.replace_source(self.source_viewport, self.node_viewport)
    }

    fn require_full_frame(&mut self, reason: u64) {
        self.force_full_frame = true;
        self.accumulated_damage_pixels = 0;
        self.recovery_reason = Some(reason);
        self.observe_recovery("requested", reason);
    }

    fn take_full_frame_request(&mut self) -> Option<u64> {
        self.sender
            .as_mut()
            .and_then(MediaSender::take_full_frame_request)
    }

    fn take_source_event(&mut self) -> Option<SourceEvent> {
        self.sender.as_mut().and_then(MediaSender::take_event)
    }

    fn teardown(&mut self) -> io::Result<()> {
        if self.torn_down {
            return Ok(());
        }
        self.torn_down = true;
        let mut first_error = None;
        if let Err(error) = self.session.delete_scene_node(self.node_id) {
            first_error = Some(error);
        }
        let sender = self.sender.take();
        if let Err(error) = self.session.destroy_source(self.source_id)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        // As in source replacement, the media transport must outlive DESTROY_SOURCE.
        drop(sender);
        if let Err(error) = self.session.goodbye()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn create_viewport_source(
    session: &mut ProducerSession,
    source_id: u64,
    viewport: WindowSize,
    capture_policy: u64,
    descriptor: &SourceDescriptor,
) -> io::Result<SourceHandle> {
    if session.supports(messages::FEATURE_RASTER_DELTA_V1) {
        session.create_raster_delta_source_with_options(
            source_id,
            viewport.page_area_width_px(),
            viewport.page_area_height_px(),
            REQUESTED_DELTA_OPERATION_LIMIT,
            capture_policy,
            Some(descriptor),
            &Default::default(),
        )
    } else {
        session.create_raster_source_with_options(
            source_id,
            viewport.page_area_width_px(),
            viewport.page_area_height_px(),
            capture_policy,
            Some(descriptor),
            &Default::default(),
        )
    }
}

fn accumulated_damage_exceeds_fraction(
    accumulated: u64,
    next_damage: u64,
    frame_pixels: u64,
) -> bool {
    accumulated.saturating_add(next_damage) > frame_pixels / ACCUMULATED_DAMAGE_DENOMINATOR
}

impl Drop for VividPresenter {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}

fn scene_config(
    context_id: u64,
    node_id: u64,
    source_id: u64,
    viewport: WindowSize,
    visible: bool,
) -> SceneNodeConfig {
    SceneNodeConfig {
        node_id,
        source_id,
        context_id,
        x: 0,
        y: 0,
        width: i64::from(viewport.cols) << 32,
        height: i64::from(viewport.page_rows()) << 32,
        text_layer: messages::TEXT_LAYER_BETWEEN_BACKGROUND_AND_GLYPH,
        z_index: 0,
        visible,
        anchor_id: None,
        clip: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use super::*;
    use vivid_protocol::wire::{HEADER_SIZE, PREFACE_SIZE, Record, RecordHeader};
    use vivid_sdk::ProducerConfig;

    fn read_client_record(stream: &mut TcpStream) -> io::Result<Record> {
        let mut header = [0_u8; HEADER_SIZE];
        stream.read_exact(&mut header)?;
        let header = RecordHeader::decode(header);
        let mut body = vec![0_u8; header.body_length as usize];
        stream.read_exact(&mut body)?;
        Ok(Record {
            record_type: header.record_type,
            flags: header.flags,
            object_id: header.object_id,
            sequence: header.sequence,
            body,
        })
    }

    fn write_server_record(
        stream: &mut TcpStream,
        sequence: &mut u64,
        record_type: u16,
        object_id: u64,
        body: &[u8],
    ) -> io::Result<()> {
        *sequence += 1;
        stream.write_all(
            &RecordHeader {
                body_length: body.len() as u32,
                record_type,
                flags: 0,
                object_id,
                sequence: *sequence,
            }
            .encode(),
        )?;
        stream.write_all(body)?;
        stream.flush()
    }

    fn acknowledge_scene_transaction(
        control: &mut TcpStream,
        sequence: &mut u64,
        action: u16,
    ) -> io::Result<()> {
        for expected in [messages::BEGIN_TXN, action, messages::COMMIT_TXN] {
            let record = read_client_record(control)?;
            assert_eq!(record.record_type, expected);
            let request_id = messages::request_id(&record.body)?;
            let response = if expected == messages::COMMIT_TXN {
                messages::PRESENTED
            } else {
                messages::OK
            };
            write_server_record(
                control,
                sequence,
                response,
                record.object_id,
                &messages::ok(request_id),
            )?;
        }
        Ok(())
    }

    fn accept_source(
        listener: &TcpListener,
        control: &mut TcpStream,
        sequence: &mut u64,
        ticket_byte: u8,
    ) -> io::Result<(u64, TcpStream)> {
        let create = read_client_record(control)?;
        assert_eq!(create.record_type, messages::CREATE_RASTER);
        let source_id = create.object_id;
        let request_id = messages::request_id(&create.body)?;
        write_server_record(
            control,
            sequence,
            messages::SOURCE_READY,
            source_id,
            &messages::source_ready(
                request_id,
                source_id,
                &[ticket_byte; 32],
                messages::Credits {
                    bytes: 4096,
                    packets: 1,
                    fragments: 0,
                },
                4096,
            ),
        )?;

        let (mut media, _) = listener.accept()?;
        let mut preface = [0_u8; PREFACE_SIZE];
        media.read_exact(&mut preface)?;
        let attach = read_client_record(&mut media)?;
        assert_eq!(attach.record_type, messages::ATTACH_CHANNEL);
        assert_eq!(attach.object_id, source_id);
        Ok((source_id, media))
    }

    fn observe_media_close(
        mut media: TcpStream,
    ) -> (mpsc::Receiver<()>, thread::JoinHandle<io::Result<()>>) {
        let (closed_tx, closed_rx) = mpsc::channel();
        let observer = thread::spawn(move || {
            let mut byte = [0_u8; 1];
            loop {
                match media.read(&mut byte) {
                    Ok(0) => {
                        let _ = closed_tx.send(());
                        return Ok(());
                    }
                    Ok(_) => {}
                    Err(error) => return Err(error),
                }
            }
        });
        (closed_rx, observer)
    }

    fn acknowledge_source_retirement(
        control: &mut TcpStream,
        sequence: &mut u64,
        source_id: u64,
        closed: &mpsc::Receiver<()>,
    ) -> io::Result<bool> {
        // Give an incorrectly dropped sender enough time to close on the independent media
        // connection before servicing the pending control request, as vvmux's media worker does.
        let closed_before_destroy = closed.recv_timeout(Duration::from_millis(100)).is_ok();
        let destroy = read_client_record(control)?;
        assert_eq!(destroy.record_type, messages::DESTROY_SOURCE);
        assert_eq!(destroy.object_id, source_id);
        let request_id = messages::request_id(&destroy.body)?;
        let (record_type, body) = if closed_before_destroy {
            (
                messages::ERROR,
                messages::error(
                    request_id,
                    // vvmux reports its synchronous missing-source validation as BAD_MESSAGE,
                    // which is the exact "presenter error 5" from the resize regression.
                    messages::ERROR_BAD_MESSAGE,
                    "source does not exist",
                ),
            )
        } else {
            (messages::OK, messages::ok(request_id))
        };
        write_server_record(control, sequence, record_type, source_id, &body)?;
        if !closed_before_destroy {
            closed.recv_timeout(Duration::from_secs(2)).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "media connection remained open after DESTROY_SOURCE",
                )
            })?;
        }
        Ok(closed_before_destroy)
    }

    fn run_ordering_presenter(listener: TcpListener) -> io::Result<(bool, bool)> {
        let (mut control, _) = listener.accept()?;
        control.set_read_timeout(Some(Duration::from_secs(5)))?;
        control.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut preface = [0_u8; PREFACE_SIZE];
        control.read_exact(&mut preface)?;
        let hello = read_client_record(&mut control)?;
        assert_eq!(hello.record_type, messages::HELLO);
        let hello_request = messages::request_id(&hello.body)?;
        let features = [
            messages::FEATURE_RASTER_RGBA8,
            messages::FEATURE_SCENE_TRANSACTIONS,
            messages::FEATURE_GRID_CELL_NODES,
            messages::FEATURE_CREDIT_FLOW_CONTROL,
            messages::FEATURE_SOURCE_DESCRIPTOR_V1,
        ];
        let mut sequence = 0;
        write_server_record(
            &mut control,
            &mut sequence,
            messages::WELCOME,
            0,
            &messages::welcome(
                hello_request,
                1,
                &[1; 16],
                1,
                messages::DisplayChanged {
                    display_generation: 1,
                    viewport_width: 80,
                    viewport_height: 25,
                    grid_columns: 4,
                    grid_rows: 5,
                    cell_width: 1,
                    cell_height: 1,
                    settled: true,
                },
                &features,
            ),
        )?;

        let (first_source, first_media) = accept_source(&listener, &mut control, &mut sequence, 1)?;
        let (first_closed, first_observer) = observe_media_close(first_media);
        acknowledge_scene_transaction(&mut control, &mut sequence, messages::CREATE_NODE)?;

        let (second_source, second_media) =
            accept_source(&listener, &mut control, &mut sequence, 2)?;
        let (second_closed, second_observer) = observe_media_close(second_media);
        acknowledge_scene_transaction(&mut control, &mut sequence, messages::UPDATE_NODE)?;
        let replacement_closed_early = acknowledge_source_retirement(
            &mut control,
            &mut sequence,
            first_source,
            &first_closed,
        )?;
        first_observer.join().unwrap()?;

        acknowledge_scene_transaction(&mut control, &mut sequence, messages::DELETE_NODE)?;
        let teardown_closed_early = acknowledge_source_retirement(
            &mut control,
            &mut sequence,
            second_source,
            &second_closed,
        )?;
        second_observer.join().unwrap()?;

        let goodbye = read_client_record(&mut control)?;
        assert_eq!(goodbye.record_type, messages::GOODBYE);
        let request_id = messages::request_id(&goodbye.body)?;
        write_server_record(
            &mut control,
            &mut sequence,
            messages::OK,
            0,
            &messages::ok(request_id),
        )?;
        Ok((replacement_closed_early, teardown_closed_early))
    }

    fn live_session(endpoint: String) -> ProducerSession {
        ProducerSession::connect(&ProducerConfig {
            endpoint: Some(endpoint),
            bulk_endpoint: None,
            token: Some("00".repeat(32)),
            dry_run: false,
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
            optional_features: vec![messages::FEATURE_SOURCE_DESCRIPTOR_V1],
            authentication_kind: messages::AUTHENTICATION_WINDOW_ROOT,
            allow_version_retry: false,
        })
        .unwrap()
    }

    #[test]
    fn scene_node_is_pane_local_and_excludes_status_row() {
        let viewport = WindowSize::from_cells(100, 30, 9, 18);
        let node = scene_config(7, 8, 9, viewport, true);
        assert_eq!(node.context_id, 7);
        assert_eq!(node.x, 0);
        assert_eq!(node.y, 0);
        assert_eq!(node.width, 100_i64 << 32);
        assert_eq!(node.height, 29_i64 << 32);
        assert_eq!(node.anchor_id, None);
    }

    #[test]
    fn drag_resize_replaces_the_fixed_source_once_when_settled() {
        let source = WindowSize::from_cells(80, 24, 10, 20);
        let middle = WindowSize::from_cells(90, 26, 10, 20);
        let final_size = WindowSize::from_cells(100, 30, 10, 20);
        assert_eq!(
            resize_action(source, source, middle, false),
            ResizeAction::UpdateNode
        );
        assert_eq!(
            resize_action(source, middle, final_size, false),
            ResizeAction::UpdateNode
        );
        assert_eq!(
            resize_action(source, final_size, final_size, true),
            ResizeAction::ReplaceSource
        );
        assert_eq!(
            resize_action(final_size, final_size, final_size, true),
            ResizeAction::None
        );
    }

    #[test]
    fn resize_and_teardown_destroy_sources_before_closing_media_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("tcp:{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || run_ordering_presenter(listener));
        let viewport = WindowSize::from_cells(4, 5, 1, 1);
        let resized = WindowSize::from_cells(5, 5, 1, 1);
        let descriptor = SourceDescriptor {
            role: messages::SOURCE_ROLE_DOCUMENT,
            title: "test".to_owned(),
            content_revision: 1,
            semantic_availability: 0,
            locator: String::new(),
        };
        let mut presenter =
            VividPresenter::new(live_session(endpoint), viewport, 0, descriptor, 0).unwrap();

        let resize = presenter.resize(resized, true);
        let teardown = presenter.teardown();
        let (replacement_closed_early, teardown_closed_early) = server.join().unwrap().unwrap();

        assert!(resize.is_ok(), "resize failed: {resize:?}");
        assert!(teardown.is_ok(), "teardown failed: {teardown:?}");
        assert!(
            !replacement_closed_early,
            "replacement media closed before DESTROY_SOURCE"
        );
        assert!(
            !teardown_closed_early,
            "teardown media closed before DESTROY_SOURCE"
        );
    }

    #[test]
    fn command_queue_capacity_obeys_both_advertised_windows() {
        assert_eq!(
            command_queue_capacity(
                MediaQueueLimits {
                    max_bytes: 8 * 1024,
                    max_packets: 8
                },
                2 * 1024,
            ),
            4
        );
        assert_eq!(
            command_queue_capacity(
                MediaQueueLimits {
                    max_bytes: 1,
                    max_packets: 64
                },
                4096
            ),
            1
        );
    }

    #[test]
    fn accumulated_damage_forces_a_full_frame_only_after_half_area() {
        assert!(!accumulated_damage_exceeds_fraction(20, 30, 100));
        assert!(accumulated_damage_exceeds_fraction(20, 31, 100));
        assert!(accumulated_damage_exceeds_fraction(
            u64::MAX,
            u64::MAX,
            u64::MAX
        ));
    }

    #[test]
    fn source_epoch_and_recovery_paths_reestablish_a_full_frame() {
        let session = ProducerSession::connect(&ProducerConfig {
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
        .unwrap();
        let viewport = WindowSize::from_cells(4, 5, 1, 1);
        let descriptor = SourceDescriptor {
            role: messages::SOURCE_ROLE_DOCUMENT,
            title: "test".to_owned(),
            content_revision: 1,
            semantic_availability: 0,
            locator: String::new(),
        };
        let mut presenter = VividPresenter::new(session, viewport, 0, descriptor, 0).unwrap();
        assert_eq!(
            presenter.delta_operation_limit(),
            Some(REQUESTED_DELTA_OPERATION_LIMIT)
        );

        let frame = vec![0_u8; viewport.framebuffer_len().unwrap()];
        presenter.show_frame(&frame, None).unwrap();
        assert!(!presenter.force_full_frame);
        assert_eq!(presenter.accumulated_damage_pixels, 0);

        let pixel = vec![1, 2, 3, 255];
        let mut changed_frame = frame.clone();
        changed_frame[..4].copy_from_slice(&pixel);
        let delta = FrameDelta {
            operations: vec![DeltaOperation::Overwrite {
                rect: crate::compositor::DamageRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                rgba: pixel,
            }],
            damaged_pixels: 1,
        };
        presenter.show_frame(&changed_frame, Some(&delta)).unwrap();
        assert_eq!(presenter.accumulated_damage_pixels, 1);

        presenter.require_full_frame(messages::NEED_FULL_FRAME_BASE_UNAVAILABLE);
        assert!(presenter.force_full_frame);
        presenter.show_frame(&changed_frame, Some(&delta)).unwrap();
        assert!(!presenter.force_full_frame);
        assert_eq!(presenter.accumulated_damage_pixels, 0);

        let resized = WindowSize::from_cells(5, 5, 1, 1);
        presenter.resize(resized, true).unwrap();
        assert_eq!(presenter.epoch, 2);
        assert_eq!(presenter.frame_id, 0);
        assert!(presenter.force_full_frame);
    }
}
