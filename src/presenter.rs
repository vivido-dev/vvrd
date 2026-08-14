use std::collections::VecDeque;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use vivid_protocol::{
    cbor::Value,
    media::{self, RasterDeltaOperation},
    messages::LaneClass,
    registry,
    track::{KindConfiguration, TrackConfiguration, TrackMode},
};
use vivid_sdk::{
    ChannelEvent, CoordinateModel, Fit, GENERIC_CONTENT, MILESTONE_OUTPUT_READY, OBSERVABILITY,
    RasterConfiguration, RequestMetadata, SceneNode, SessionEvent, SlotBinding, Surface,
    SurfaceDefinition, SurfaceDescriptor, Track, TrackChannel, TrackWaitCondition,
};

use crate::{
    compositor::{DeltaOperation, FrameDelta},
    geometry::{TargetViewport, WindowSize},
};

/// Actionable presenter traffic, already scoped to this producer's own objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenterSignal {
    /// The active channel needs a full frame before it accepts another delta.
    FullFrameNeeded(u64),
    /// The active raster track was lost. The surface, its node, and its identity survive.
    TrackLost(String),
    /// The track transport failed. A new channel generation recovers it under the same track.
    ChannelLost(String),
    /// The presentation target changed; the caller re-reads the target descriptor.
    TargetChanged,
    /// The control connection is gone. Nothing further can be submitted.
    ConnectionClosed(String),
}

pub trait Presenter {
    fn show_frame(&mut self, rgba: &[u8], delta: Option<&FrameDelta>) -> io::Result<u64>;
    fn set_visible(&mut self, visible: bool) -> io::Result<()>;
    fn resize(&mut self, viewport: WindowSize, settled: bool) -> io::Result<()>;
    fn recover_track(&mut self) -> io::Result<()>;
    fn recover_channel(&mut self, reason: u64) -> io::Result<()>;
    fn require_full_frame(&mut self, reason: u64);
    fn take_signal(&mut self) -> Option<PresenterSignal>;
    fn teardown(&mut self) -> io::Result<()>;
}

/// The one surface slot a document reader occupies. Slot 3 is `raster` in the 1.5 media registry.
const SLOT_RASTER: u64 = 3;
/// Terminal text layer: between the terminal background and its glyphs.
const TEXT_LAYER_BETWEEN_BACKGROUND_AND_GLYPH: u64 = 1;
/// Grid-cell coordinate space in the terminal-surface node geometry schema.
const COORDINATE_SPACE_GRID_CELL: u64 = 1;
const REQUESTED_DELTA_OPERATIONS: u8 = media::RASTER_DELTA_OPERATION_LIMIT as u8;
const ACCUMULATED_DAMAGE_DENOMINATOR: u64 = 2;
/// Contractual claims. A document reader is event-driven, so these bound bursts, not a stream.
const MAXIMUM_FRAME_RATE_MILLIHERTZ: u64 = 30_000;
const MAXIMUM_RECORDS_PER_SECOND: u64 = 30;
const FULL_FRAMES_PER_SECOND_CLAIM: u64 = 4;
const INFLIGHT_FRAME_CLAIM: u64 = 4;
const MAXIMUM_LATENCY_US: u64 = 1_000_000;
const PRIME_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a scene commit keeps following a moving presentation target before it gives up.
const TARGET_FOLLOW_TIMEOUT: Duration = Duration::from_secs(2);
/// Pause between attempts while the announcement that explains a stale reply is still in flight.
const TARGET_FOLLOW_POLL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RasterSendKind {
    Full,
    Delta,
}

pub struct VividPresenter {
    session: vivid_sdk::Session,
    surface: Surface,
    node_id: u64,
    track: Track,
    channel: Option<TrackChannel>,
    /// Geometry the active raster track was configured for. Raster dimensions are immutable, so
    /// this only changes when the track is replaced.
    track_viewport: WindowSize,
    /// Geometry the scene node currently claims, in terminal cells.
    node_viewport: WindowSize,
    visible: bool,
    epoch: u32,
    /// Last media ID accepted by the active track. Media IDs increase across channel generations
    /// and restart only with a replacement track.
    frame_id: u64,
    force_full_frame: bool,
    accumulated_damage_pixels: u64,
    recovery_reason: Option<u64>,
    torn_down: bool,
    descriptor: SurfaceDescriptor,
    semantic_state: (usize, Option<String>, u64),
    signals: VecDeque<PresenterSignal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeAction {
    None,
    UpdateNode,
    ReplaceTrack,
}

fn resize_action(
    track_viewport: WindowSize,
    node_viewport: WindowSize,
    viewport: WindowSize,
    settled: bool,
) -> ResizeAction {
    if settled {
        if viewport == track_viewport && viewport == node_viewport {
            ResizeAction::None
        } else {
            ResizeAction::ReplaceTrack
        }
    } else if viewport == node_viewport {
        ResizeAction::None
    } else {
        ResizeAction::UpdateNode
    }
}

impl VividPresenter {
    pub fn new(
        mut session: vivid_sdk::Session,
        viewport: WindowSize,
        policy: u64,
        descriptor: SurfaceDescriptor,
        initial_page: usize,
    ) -> io::Result<Self> {
        let context_id = session.info().root_context_id;
        let surface_id = session.allocate_id()?;
        let node_id = session.allocate_id()?;
        let surface = session.create_surface(
            document_surface(context_id, surface_id, viewport, policy, descriptor.clone()),
            &RequestMetadata::default(),
        )?;

        let (track, channel) = match create_and_prime_track(&mut session, &surface, viewport) {
            Ok(primed) => primed,
            Err(error) => {
                let _ = session.destroy_surface(&surface, &RequestMetadata::default());
                return Err(error);
            }
        };
        if let Err(error) = activate_raster_slot(&mut session, &surface, &track) {
            let _ = session.destroy_track(&track, &RequestMetadata::default());
            drop(channel);
            let _ = session.destroy_surface(&surface, &RequestMetadata::default());
            return Err(error);
        }
        let mut signals = VecDeque::new();
        if let Err(error) = commit_node_following_target(
            &mut session,
            &track,
            &mut signals,
            NodeCommit::Create(&terminal_node(
                context_id, node_id, &surface, viewport, true,
            )),
        ) {
            let _ = session.destroy_track(&track, &RequestMetadata::default());
            drop(channel);
            let _ = session.destroy_surface(&surface, &RequestMetadata::default());
            return Err(error);
        }

        Ok(Self {
            session,
            surface,
            node_id,
            track,
            channel: Some(channel),
            track_viewport: viewport,
            node_viewport: viewport,
            visible: true,
            epoch: 1,
            // The priming full frame already used media ID one.
            frame_id: 1,
            force_full_frame: false,
            accumulated_damage_pixels: 0,
            recovery_reason: None,
            torn_down: false,
            descriptor,
            semantic_state: (initial_page, None, 1),
            signals,
        })
    }

    /// Replace the active raster track without disturbing surface, node, or input identity.
    ///
    /// The replacement is primed and proven output-ready before the atomic slot activation, so the
    /// swap never exposes an unrendered track. This is the 1.5 behaviour that removed the blank
    /// frame the 1.1 source replacement produced.
    fn replace_track(
        &mut self,
        track_viewport: WindowSize,
        node_viewport: WindowSize,
    ) -> io::Result<()> {
        let (track, channel) =
            create_and_prime_track(&mut self.session, &self.surface, track_viewport)?;

        if track_viewport != self.track_viewport {
            let mut replacement = self.surface.definition()?;
            replacement.logical_width = u64::from(track_viewport.page_area_width_px());
            replacement.logical_height = u64::from(track_viewport.page_area_height_px());
            if let Err(error) =
                self.session
                    .update_surface(&self.surface, replacement, &RequestMetadata::default())
            {
                let _ = self
                    .session
                    .destroy_track(&track, &RequestMetadata::default());
                drop(channel);
                return Err(error);
            }
        }

        if let Err(error) = activate_raster_slot(&mut self.session, &self.surface, &track) {
            let _ = self
                .session
                .destroy_track(&track, &RequestMetadata::default());
            drop(channel);
            return Err(error);
        }

        if node_viewport != self.node_viewport {
            let context_id = self.session.info().root_context_id;
            let node = terminal_node(
                context_id,
                self.node_id,
                &self.surface,
                node_viewport,
                self.visible,
            );
            self.commit_node(NodeCommit::Update(&node))?;
            self.node_viewport = node_viewport;
        }

        let retired_track = std::mem::replace(&mut self.track, track);
        let retired_channel = self.channel.replace(channel);
        self.track_viewport = track_viewport;
        self.epoch = 1;
        // A replacement track owns a fresh media-ID space; the priming frame consumed ID one.
        self.frame_id = 1;
        self.force_full_frame = false;
        self.accumulated_damage_pixels = 0;
        self.recovery_reason = None;
        // The track transport must outlive the ordered DESTROY_TRACK request: a relay that sees
        // the media connection close first removes the track on EOF and then rejects the destroy
        // because the track no longer exists.
        let result = destroy_if_live(&mut self.session, &retired_track);
        drop(retired_channel);
        result
    }

    fn commit_node(&mut self, commit: NodeCommit<'_>) -> io::Result<()> {
        commit_node_following_target(&mut self.session, &self.track, &mut self.signals, commit)
    }

    pub fn delta_operation_limit(&self) -> Option<u32> {
        self.track
            .delta_operation_limit()
            .ok()
            .filter(|limit| *limit > 0)
    }

    fn observe_recovery(&mut self, stage: &str, reason: u64) {
        if !self.session.supports(OBSERVABILITY) {
            return;
        }
        match self.session.query_track(&self.track) {
            // Milestones are generation-local in 1.5, so the generation they belong to is part of
            // the observation, never an implied "current".
            Ok(status) => log::debug!(
                "raster full-frame recovery {stage}: reason={reason} context={} surface={} track={} generation={} milestones={:#x} media_id={}",
                status.context_id,
                status.surface_id,
                status.track_id,
                status.channel_generation.get(),
                status.milestones,
                status.last_media_id,
            ),
            Err(error) => log::debug!(
                "raster full-frame recovery {stage}: reason={reason} observation unavailable: {error}"
            ),
        }
    }

    fn expected_frame_len(&self) -> io::Result<usize> {
        self.track_viewport
            .framebuffer_len()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
    }

    /// Viewport the active raster track was created for.
    ///
    /// Compositions are planned against a viewport captured before they were queued, so callers
    /// must compare it against this before submitting: a settled resize replaces the track, and
    /// `show_frame` rejects any frame that no longer matches it.
    pub fn track_viewport(&self) -> WindowSize {
        self.track_viewport
    }

    /// Current terminal target and settle state, from the presenter's target descriptor.
    pub fn target_viewport(&self) -> io::Result<(TargetViewport, bool)> {
        WindowSize::from_target_descriptor(&self.session.info().target_descriptor)
    }

    pub fn command_queue_capacity(&self) -> usize {
        let inflight = self
            .track
            .configuration()
            .map(|configuration| configuration.maximum_inflight_body_bytes)
            .unwrap_or(1);
        command_queue_capacity(inflight, self.expected_frame_len().unwrap_or(usize::MAX))
    }

    pub fn update_content_descriptor(
        &mut self,
        page: usize,
        search_term: Option<String>,
        document_revision: u64,
    ) -> io::Result<()> {
        if self.semantic_state == (page, search_term.clone(), document_revision) {
            return Ok(());
        }
        self.descriptor.semantic_content_revision = self
            .descriptor
            .semantic_content_revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("document content revision space exhausted"))?;
        let mut replacement = self.surface.definition()?;
        replacement.descriptor = self.descriptor.clone();
        // A descriptor-only update advances the surface revision but not the surface generation:
        // coordinate and input target truth did not change.
        self.session
            .update_surface(&self.surface, replacement, &RequestMetadata::default())?;
        self.semantic_state = (page, search_term, document_revision);
        Ok(())
    }

    /// Drain the control connection and the active track channel into scoped signals.
    fn poll_presenter(&mut self) -> io::Result<()> {
        drain_session_events(&mut self.session, &self.track, &mut self.signals)?;
        let channel_events: Vec<_> = match self.channel.as_ref() {
            Some(channel) => {
                let mut events = Vec::new();
                while let Some(event) = channel.take_event()? {
                    events.push(event);
                }
                events
            }
            None => Vec::new(),
        };
        for event in channel_events {
            match event {
                ChannelEvent::NeedFullFrame(payload) | ChannelEvent::NeedKeyframe(payload) => {
                    let reason = payload
                        .iter()
                        .find(|(key, _)| *key == 3)
                        .and_then(|(_, value)| value.as_u64())
                        .unwrap_or(0);
                    self.signals
                        .push_back(PresenterSignal::FullFrameNeeded(reason));
                }
                ChannelEvent::Error(error) => {
                    self.signals.push_back(PresenterSignal::ChannelLost(format!(
                        "track channel error {}",
                        error.code
                    )));
                }
            }
        }
        Ok(())
    }

    /// Turn a transport-level send failure into an actionable channel-loss signal.
    ///
    /// The SDK reports a dead track connection only when a send fails, so without this a recoverable
    /// transport failure would reach the reader as a fatal error. The track and the surface are both
    /// still valid; only this channel generation is gone.
    fn classify_send_failure(&mut self, error: io::Error) -> io::Error {
        if matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::NotConnected
                | io::ErrorKind::UnexpectedEof
        ) {
            self.signals
                .push_back(PresenterSignal::ChannelLost(error.to_string()));
        }
        error
    }
}

/// One scene mutation, named so a stale reply can be retried against the target that caused it.
#[derive(Debug, Clone, Copy)]
enum NodeCommit<'a> {
    Create(&'a SceneNode),
    Update(&'a SceneNode),
    Delete { context_id: u64, node_id: u64 },
}

/// Run one scene commit, following the presentation target while it moves.
///
/// A commit names the target generation it was planned against, so a resize that lands between
/// planning and committing is answered with `STALE_TARGET_GENERATION`. That is not a failure: the
/// presenter announces every change that causes one, so apply what has arrived and commit again
/// against the target the terminal has now. A drag produces a generation per frame, so this
/// follows the target until it stops moving rather than retrying a fixed number of times.
fn commit_node_following_target(
    session: &mut vivid_sdk::Session,
    track: &Track,
    signals: &mut VecDeque<PresenterSignal>,
    commit: NodeCommit<'_>,
) -> io::Result<()> {
    let deadline = Instant::now() + TARGET_FOLLOW_TIMEOUT;
    loop {
        let metadata = RequestMetadata::default();
        let attempt = match commit {
            NodeCommit::Create(node) => session.create_node(node, &metadata).map(|_| ()),
            NodeCommit::Update(node) => session.update_node(node, &metadata).map(|_| ()),
            NodeCommit::Delete {
                context_id,
                node_id,
            } => session
                .delete_node(context_id, node_id, &metadata)
                .map(|_| ()),
        };
        let stale = match attempt {
            Ok(()) => return Ok(()),
            Err(error)
                if presenter_code(&error) == Some(registry::error::STALE_TARGET_GENERATION) =>
            {
                error
            }
            Err(error) => return Err(error),
        };
        if !drain_session_events(session, track, signals)? {
            // The announcement that explains the rejection has not arrived yet.
            if Instant::now() >= deadline {
                return Err(stale);
            }
            thread::sleep(TARGET_FOLLOW_POLL);
        }
    }
}

/// Drain the control connection into scoped signals, reporting whether the target moved.
fn drain_session_events(
    session: &mut vivid_sdk::Session,
    track: &Track,
    signals: &mut VecDeque<PresenterSignal>,
) -> io::Result<bool> {
    let mut target_moved = false;
    while let Some(event) = session.take_event()? {
        target_moved |= record_signal(session, track, signals, event);
    }
    Ok(target_moved)
}

/// Turn one session event into a scoped signal, reporting whether the target generation advanced.
fn record_signal(
    session: &mut vivid_sdk::Session,
    track: &Track,
    signals: &mut VecDeque<PresenterSignal>,
    event: SessionEvent,
) -> bool {
    match event {
        SessionEvent::TargetChanged(payload) => match session.apply_target_changed(&payload) {
            Ok(_) => {
                signals.push_back(PresenterSignal::TargetChanged);
                return true;
            }
            Err(error) => log::debug!("ignored unusable TARGET_CHANGED: {error}"),
        },
        SessionEvent::TrackLost { object_id, payload } => {
            // Track loss is matched by complete owner identity. Another producer, or another
            // context in this session, may legitimately reuse this numeric track ID.
            let field = |key: u64| {
                payload
                    .iter()
                    .find(|(candidate, _)| *candidate == key)
                    .and_then(|(_, value)| value.as_u64())
            };
            let Ok(configuration) = track.configuration() else {
                return false;
            };
            if field(0) != Some(configuration.context_id)
                || field(1) != Some(configuration.surface_id)
                || field(2) != Some(configuration.track_id)
                || object_id != configuration.track_id
            {
                return false;
            }
            let code = field(3).unwrap_or(0);
            signals.push_back(PresenterSignal::TrackLost(format!(
                "raster track {} was lost (code {code})",
                configuration.track_id
            )));
        }
        SessionEvent::ConnectionClosed { diagnostic } => {
            signals.push_back(PresenterSignal::ConnectionClosed(diagnostic));
        }
        SessionEvent::AnchorReady { .. }
        | SessionEvent::AnchorGone { .. }
        | SessionEvent::ContextChanged { .. }
        | SessionEvent::FileDropOffered(_)
        | SessionEvent::FileDropCancelled(_)
        | SessionEvent::Other { .. } => {}
    }
    false
}

fn command_queue_capacity(maximum_inflight_body_bytes: u64, frame_bytes: usize) -> usize {
    let frames = maximum_inflight_body_bytes / (frame_bytes.max(1) as u64);
    usize::try_from(frames).unwrap_or(usize::MAX).max(1)
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
        let channel = self
            .channel
            .as_ref()
            .ok_or_else(|| io::Error::other("raster track channel is unavailable"))?;
        let zstd_enabled = matches!(
            self.track.configuration()?.kind,
            KindConfiguration::Raster(raster) if raster.zstd_enabled
        );
        let frame_id = self
            .frame_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("raster frame ID space exhausted"))?;
        let width = self.track_viewport.page_area_width_px();
        let height = self.track_viewport.page_area_height_px();
        let frame_pixels = u64::from(width) * u64::from(height);
        let delta_allowed = delta.is_some_and(|delta| {
            !self.force_full_frame
                && self.delta_operation_limit().is_some()
                && !accumulated_damage_exceeds_fraction(
                    self.accumulated_damage_pixels,
                    delta.damaged_pixels,
                    frame_pixels,
                )
                && delta_is_cheaper_than_full(delta, rgba.len())
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
            let send_delta = if zstd_enabled {
                channel.send_raster_delta_adaptive(
                    self.epoch,
                    frame_id,
                    self.frame_id,
                    0,
                    0,
                    &operations,
                )
            } else {
                channel.send_raster_delta(
                    self.epoch,
                    frame_id,
                    self.frame_id,
                    0,
                    0,
                    &operations,
                    false,
                )
            };
            match send_delta {
                Ok(_) => RasterSendKind::Delta,
                // The channel may have been told to recover between the plan and the send. A full
                // frame is always legal, so fall back rather than dropping the composition.
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                    log::debug!("raster delta rejected, sending a full frame instead: {error}");
                    let full = if zstd_enabled {
                        channel.send_raster_adaptive(self.epoch, frame_id, rgba)
                    } else {
                        channel.send_raster(self.epoch, frame_id, rgba, false)
                    };
                    match full {
                        Ok(_) => RasterSendKind::Full,
                        Err(error) => return Err(self.classify_send_failure(error)),
                    }
                }
                Err(error) => return Err(self.classify_send_failure(error)),
            }
        } else {
            let full = if zstd_enabled {
                channel.send_raster_adaptive(self.epoch, frame_id, rgba)
            } else {
                channel.send_raster(self.epoch, frame_id, rgba, false)
            };
            match full {
                Ok(_) => RasterSendKind::Full,
                Err(error) => return Err(self.classify_send_failure(error)),
            }
        };

        self.frame_id = frame_id;
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
        let context_id = self.session.info().root_context_id;
        let node = terminal_node(
            context_id,
            self.node_id,
            &self.surface,
            self.node_viewport,
            visible,
        );
        self.commit_node(NodeCommit::Update(&node))?;
        self.visible = visible;
        Ok(())
    }

    fn resize(&mut self, viewport: WindowSize, settled: bool) -> io::Result<()> {
        match resize_action(self.track_viewport, self.node_viewport, viewport, settled) {
            ResizeAction::None => Ok(()),
            ResizeAction::UpdateNode => {
                let context_id = self.session.info().root_context_id;
                let node = terminal_node(
                    context_id,
                    self.node_id,
                    &self.surface,
                    viewport,
                    self.visible,
                );
                self.commit_node(NodeCommit::Update(&node))?;
                self.node_viewport = viewport;
                Ok(())
            }
            ResizeAction::ReplaceTrack => self.replace_track(viewport, viewport),
        }
    }

    fn recover_track(&mut self) -> io::Result<()> {
        self.replace_track(self.track_viewport, self.node_viewport)
    }

    fn recover_channel(&mut self, reason: u64) -> io::Result<()> {
        // Dropping the old transport before ADVANCE_CHANNEL is correct here, unlike track destroy:
        // the track survives, and the presenter expects the previous generation to be gone.
        self.channel = None;
        self.session
            .advance_channel(&self.track, reason, &RequestMetadata::default())?;
        self.channel = Some(self.session.open_track_channel(&self.track)?);
        // Media IDs belong to the track across channel generations, so frame_id keeps climbing.
        // Only the epoch and the recovery obligation are generation-local.
        self.epoch = self.epoch.checked_add(1).unwrap_or(1);
        self.force_full_frame = true;
        self.accumulated_damage_pixels = 0;
        self.recovery_reason = Some(reason);
        Ok(())
    }

    fn require_full_frame(&mut self, reason: u64) {
        self.force_full_frame = true;
        self.accumulated_damage_pixels = 0;
        self.recovery_reason = Some(reason);
        self.observe_recovery("requested", reason);
    }

    fn take_signal(&mut self) -> Option<PresenterSignal> {
        if let Err(error) = self.poll_presenter() {
            return Some(PresenterSignal::ConnectionClosed(error.to_string()));
        }
        self.signals.pop_front()
    }

    fn teardown(&mut self) -> io::Result<()> {
        if self.torn_down {
            return Ok(());
        }
        self.torn_down = true;
        let mut first_error = None;
        let context_id = self.session.info().root_context_id;
        let node_id = self.node_id;
        if let Err(error) = self.commit_node(NodeCommit::Delete {
            context_id,
            node_id,
        }) {
            first_error = Some(error);
        }
        let channel = self.channel.take();
        let track = self.track.clone();
        if let Err(error) = destroy_if_live(&mut self.session, &track)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        // As in track replacement, the media transport must outlive DESTROY_TRACK.
        drop(channel);
        if let Err(error) = self
            .session
            .destroy_surface(&self.surface, &RequestMetadata::default())
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Create a raster track for `viewport`, attach its channel, and prove it produces output.
///
/// The track is not activated here: a replacement must be output-ready before it takes the slot.
fn create_and_prime_track(
    session: &mut vivid_sdk::Session,
    surface: &Surface,
    viewport: WindowSize,
) -> io::Result<(Track, TrackChannel)> {
    let track_id = session.allocate_id()?;
    let mut configuration = None;
    // Compression matters more than deltas for document page turns, while deltas dominate scroll
    // and pan. Prefer both, then preserve zstd if the presenter accepts only one enhancement.
    for (delta_enabled, zstd_enabled) in
        [(true, true), (false, true), (true, false), (false, false)]
    {
        let candidate = raster_track(surface, track_id, viewport, delta_enabled, zstd_enabled)?;
        if session.probe_track(&probe_of(&candidate))?.supported {
            configuration = Some(candidate);
            break;
        }
    }
    let configuration = configuration.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "presenter rejected the viewport raster track configuration",
        )
    })?;
    let zstd_enabled = matches!(
        &configuration.kind,
        KindConfiguration::Raster(raster) if raster.zstd_enabled
    );
    let track = session.create_track(configuration, &RequestMetadata::default())?;

    let prime = (|| -> io::Result<TrackChannel> {
        let channel = session.open_track_channel(&track)?;
        let blank = vec![0_u8; viewport.framebuffer_len().map_err(io::Error::other)?];
        if zstd_enabled {
            channel.send_raster_adaptive(1, 1, &blank)?;
        } else {
            channel.send_raster(1, 1, &blank, false)?;
        }
        session.wait_track(
            &track,
            TrackWaitCondition::MilestoneSet,
            Some(MILESTONE_OUTPUT_READY),
            timeout_us(PRIME_TIMEOUT),
        )?;
        Ok(channel)
    })();
    match prime {
        Ok(channel) => Ok((track, channel)),
        Err(error) => {
            let _ = session.destroy_track(&track, &RequestMetadata::default());
            Err(error)
        }
    }
}

/// Destroy a track unless it is already terminal.
///
/// A track that reported `TRACK_LOST` is gone from the presenter's authority; asking again is not a
/// failure to report, and the surface it belonged to is unaffected either way.
fn destroy_if_live(session: &mut vivid_sdk::Session, track: &Track) -> io::Result<()> {
    match session.destroy_track(track, &RequestMetadata::default()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn activate_raster_slot(
    session: &mut vivid_sdk::Session,
    surface: &Surface,
    track: &Track,
) -> io::Result<()> {
    session
        .activate_tracks(
            surface,
            &[SlotBinding {
                slot: SLOT_RASTER,
                track_id: track.id(),
                expected_channel_generation: track.channel_generation(),
                required_milestone: MILESTONE_OUTPUT_READY,
            }],
            &RequestMetadata::default(),
        )
        .map(|_| ())
}

fn document_surface(
    context_id: u64,
    surface_id: u64,
    viewport: WindowSize,
    policy: u64,
    descriptor: SurfaceDescriptor,
) -> SurfaceDefinition {
    SurfaceDefinition {
        context_id,
        surface_id,
        semantic_profile: GENERIC_CONTENT.into(),
        coordinate_model: CoordinateModel::DesktopLogicalPixels,
        logical_width: u64::from(viewport.page_area_width_px()),
        logical_height: u64::from(viewport.page_area_height_px()),
        scale_numerator: 1,
        scale_denominator: 1,
        rotation: 0,
        descriptor,
        policy,
        profile_parameters: Vec::new(),
    }
}

fn raster_track(
    surface: &Surface,
    track_id: u64,
    viewport: WindowSize,
    delta_enabled: bool,
    zstd_enabled: bool,
) -> io::Result<TrackConfiguration> {
    let width = viewport.page_area_width_px();
    let height = viewport.page_area_height_px();
    let maximum_record_body =
        media::rgba8_raw_frame_body_len(width, height).map_err(io::Error::other)?;
    let retained_pixel_charge = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "raster pixels overflow"))?;
    Ok(TrackConfiguration {
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id,
        slot: SLOT_RASTER,
        mode: TrackMode::Live,
        lane: LaneClass::Bulk,
        maximum_record_body,
        maximum_rate_millihertz: MAXIMUM_FRAME_RATE_MILLIHERTZ,
        // Deltas carry most frames, so the sustained claim covers a few full frames per second
        // rather than the frame rate multiplied by the full-frame size.
        maximum_encoded_bits_per_second: u64::from(maximum_record_body)
            .saturating_mul(8)
            .saturating_mul(FULL_FRAMES_PER_SECOND_CLAIM)
            .max(1),
        maximum_records_per_second: MAXIMUM_RECORDS_PER_SECOND,
        maximum_inflight_body_bytes: u64::from(maximum_record_body)
            .saturating_mul(INFLIGHT_FRAME_CLAIM),
        kind: KindConfiguration::Raster(RasterConfiguration {
            width,
            height,
            alpha_mode: 1,
            delta_enabled,
            maximum_delta_operations: if delta_enabled {
                REQUESTED_DELTA_OPERATIONS
            } else {
                1
            },
            zstd_enabled,
        }),
        target_latency_us: 0,
        maximum_latency_us: MAXIMUM_LATENCY_US,
        retained_pixel_charge,
    })
}

fn probe_of(configuration: &TrackConfiguration) -> TrackConfiguration {
    let mut probe = configuration.clone();
    probe.track_id = 0;
    probe
}

fn terminal_node(
    context_id: u64,
    node_id: u64,
    surface: &Surface,
    viewport: WindowSize,
    visible: bool,
) -> SceneNode {
    SceneNode {
        owning_context_id: context_id,
        node_id,
        surface_context_id: surface.context_id(),
        surface_id: surface.id(),
        geometry: vec![
            (0, Value::Unsigned(COORDINATE_SPACE_GRID_CELL)),
            (1, Value::Unsigned(0)),
            (2, Value::Unsigned(0)),
            (3, Value::Unsigned(u64::from(viewport.cols) << 32)),
            (4, Value::Unsigned(u64::from(viewport.page_rows()) << 32)),
            (5, Value::Unsigned(TEXT_LAYER_BETWEEN_BACKGROUND_AND_GLYPH)),
        ],
        fit: Fit::Contain,
        linear_sampling: true,
        z_index: 0,
        visible,
        opacity: u16::MAX,
        clip: None,
    }
}

fn timeout_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn presenter_code(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<vivid_sdk::PresenterError>())
        .map(|error| error.code)
}

fn accumulated_damage_exceeds_fraction(
    accumulated: u64,
    next_damage: u64,
    frame_pixels: u64,
) -> bool {
    accumulated.saturating_add(next_damage) > frame_pixels / ACCUMULATED_DAMAGE_DENOMINATOR
}

/// Upper bound on the delta body, used to keep a delta from costing more than the full frame.
///
/// Vivid 1.5 has no `send_raster_delta_or_full`; the producer owns this choice.
fn delta_is_cheaper_than_full(delta: &FrameDelta, full_frame_bytes: usize) -> bool {
    const DELTA_PREFIX_BYTES: usize = 48;
    const DELTA_OPERATION_BYTES: usize = 32;
    let mut bytes = DELTA_PREFIX_BYTES;
    for operation in &delta.operations {
        bytes = bytes.saturating_add(DELTA_OPERATION_BYTES);
        if let DeltaOperation::Overwrite { rgba, .. } = operation {
            bytes = bytes.saturating_add(rgba.len());
        }
    }
    bytes < full_frame_bytes
}

impl Drop for VividPresenter {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::registry;
    use vivid_sdk::{ProducerConfig, Session, SurfaceRole};

    fn offline_session() -> Session {
        Session::connect(ProducerConfig::offline()).unwrap()
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

    #[test]
    fn scene_node_is_context_local_and_excludes_the_status_row() {
        let mut session = offline_session();
        let surface = session
            .create_surface(
                document_surface(
                    7,
                    9,
                    WindowSize::from_cells(100, 30, 9, 18),
                    0,
                    test_descriptor(),
                ),
                &RequestMetadata::default(),
            )
            .unwrap();
        let viewport = WindowSize::from_cells(100, 30, 9, 18);
        let node = terminal_node(7, 8, &surface, viewport, true);
        assert_eq!(node.owning_context_id, 7);
        assert_eq!(node.surface_context_id, 7);
        assert_eq!(node.surface_id, 9);
        assert_eq!(node.geometry[1].1, Value::Unsigned(0));
        assert_eq!(node.geometry[2].1, Value::Unsigned(0));
        assert_eq!(node.geometry[3].1, Value::Unsigned(100 << 32));
        assert_eq!(node.geometry[4].1, Value::Unsigned(29 << 32));
    }

    #[test]
    fn drag_resize_replaces_the_fixed_track_once_when_settled() {
        let track = WindowSize::from_cells(80, 24, 10, 20);
        let middle = WindowSize::from_cells(90, 26, 10, 20);
        let final_size = WindowSize::from_cells(100, 30, 10, 20);
        assert_eq!(
            resize_action(track, track, middle, false),
            ResizeAction::UpdateNode
        );
        assert_eq!(
            resize_action(track, middle, final_size, false),
            ResizeAction::UpdateNode
        );
        assert_eq!(
            resize_action(track, final_size, final_size, true),
            ResizeAction::ReplaceTrack
        );
        assert_eq!(
            resize_action(final_size, final_size, final_size, true),
            ResizeAction::None
        );
    }

    #[test]
    fn command_queue_capacity_obeys_the_declared_inflight_claim() {
        assert_eq!(command_queue_capacity(8 * 1024, 2 * 1024), 4);
        assert_eq!(command_queue_capacity(1, 4096), 1);
    }

    #[test]
    fn viewport_track_prefers_delta_and_zstd_when_the_presenter_supports_both() {
        let viewport = WindowSize::from_cells(80, 24, 10, 20);
        let presenter =
            VividPresenter::new(offline_session(), viewport, 0, test_descriptor(), 0).unwrap();
        let configuration = presenter.track.configuration().unwrap();
        let KindConfiguration::Raster(raster) = configuration.kind else {
            panic!("vvrd created a non-raster viewport track")
        };
        assert!(raster.delta_enabled);
        assert!(raster.zstd_enabled);
        assert_eq!(raster.maximum_delta_operations, REQUESTED_DELTA_OPERATIONS);
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
    fn a_delta_larger_than_the_full_frame_is_not_worth_sending() {
        let overwrite = |bytes: usize| FrameDelta {
            operations: vec![DeltaOperation::Overwrite {
                rect: crate::compositor::DamageRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                rgba: vec![0; bytes],
            }],
            damaged_pixels: 1,
        };
        assert!(delta_is_cheaper_than_full(&overwrite(16), 4096));
        assert!(!delta_is_cheaper_than_full(&overwrite(4096), 4096));
    }

    #[test]
    fn a_settled_resize_replaces_the_track_and_keeps_the_surface() {
        let viewport = WindowSize::from_cells(4, 5, 1, 1);
        let resized = WindowSize::from_cells(5, 5, 1, 1);
        let mut presenter =
            VividPresenter::new(offline_session(), viewport, 0, test_descriptor(), 0).unwrap();
        let surface_id = presenter.surface.id();
        let node_id = presenter.node_id;
        let generation = presenter.surface.generation();
        let first_track = presenter.track.id();

        presenter.resize(resized, true).unwrap();

        // Stable logical identity: the surface and its scene node survive the geometry change.
        assert_eq!(presenter.surface.id(), surface_id);
        assert_eq!(presenter.node_id, node_id);
        // Raster dimensions are immutable, so new geometry means a new track...
        assert_ne!(presenter.track.id(), first_track);
        // ...and the surface generation advanced because coordinate truth changed.
        assert!(presenter.surface.generation() > generation);
        assert_eq!(presenter.track_viewport(), resized);
        assert_eq!(presenter.epoch, 1);
        assert_eq!(presenter.frame_id, 1);
    }

    #[test]
    fn channel_recovery_keeps_the_track_and_climbs_the_media_id_space() {
        let viewport = WindowSize::from_cells(4, 5, 1, 1);
        let mut presenter =
            VividPresenter::new(offline_session(), viewport, 0, test_descriptor(), 0).unwrap();
        let track_id = presenter.track.id();
        let surface_id = presenter.surface.id();
        let frame = vec![0_u8; viewport.framebuffer_len().unwrap()];
        presenter.show_frame(&frame, None).unwrap();
        let before = presenter.frame_id;

        presenter
            .recover_channel(registry::error::NEED_KEYFRAME)
            .unwrap();

        assert_eq!(
            presenter.track.id(),
            track_id,
            "channel loss kept the track"
        );
        assert_eq!(presenter.surface.id(), surface_id);
        assert!(
            presenter.force_full_frame,
            "a recovered channel needs a full frame"
        );
        // Media IDs belong to the track across generations and must never restart mid-track.
        let next = presenter.show_frame(&frame, None).unwrap();
        assert!(next > before);
    }

    #[test]
    fn a_full_frame_request_disarms_only_after_a_full_frame() {
        // Large enough that a one-pixel delta is genuinely cheaper than the full frame.
        let viewport = WindowSize::from_cells(20, 6, 4, 8);
        let mut presenter =
            VividPresenter::new(offline_session(), viewport, 0, test_descriptor(), 0).unwrap();
        assert_eq!(
            presenter.delta_operation_limit(),
            Some(u32::from(REQUESTED_DELTA_OPERATIONS))
        );

        let frame = vec![0_u8; viewport.framebuffer_len().unwrap()];
        presenter.show_frame(&frame, None).unwrap();
        assert!(!presenter.force_full_frame);

        let pixel = vec![1, 2, 3, 255];
        let mut changed = frame.clone();
        changed[..4].copy_from_slice(&pixel);
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
        presenter.show_frame(&changed, Some(&delta)).unwrap();
        assert_eq!(presenter.accumulated_damage_pixels, 1);

        presenter.require_full_frame(registry::error::NEED_KEYFRAME);
        assert!(presenter.force_full_frame);
        presenter.show_frame(&changed, Some(&delta)).unwrap();
        assert!(!presenter.force_full_frame);
        assert_eq!(presenter.accumulated_damage_pixels, 0);
    }

    #[test]
    fn a_descriptor_update_does_not_advance_the_surface_generation() {
        let viewport = WindowSize::from_cells(4, 5, 1, 1);
        let mut presenter =
            VividPresenter::new(offline_session(), viewport, 0, test_descriptor(), 0).unwrap();
        let generation = presenter.surface.generation();
        let revision = presenter.surface.revision();

        presenter
            .update_content_descriptor(4, Some("term".to_owned()), 1)
            .unwrap();

        assert_eq!(presenter.surface.generation(), generation);
        assert!(presenter.surface.revision() > revision);
        assert_eq!(presenter.descriptor.semantic_content_revision, 2);
    }

    #[test]
    fn same_page_source_reload_advances_only_content_revision() {
        let viewport = WindowSize::from_cells(4, 5, 1, 1);
        let mut presenter =
            VividPresenter::new(offline_session(), viewport, 0, test_descriptor(), 0).unwrap();
        let surface_id = presenter.surface.id();
        let surface_generation = presenter.surface.generation();
        let node_id = presenter.node_id;
        let track_id = presenter.track.id();
        let track_generation = presenter.track.channel_generation();

        presenter.update_content_descriptor(0, None, 2).unwrap();

        assert_eq!(presenter.surface.id(), surface_id);
        assert_eq!(presenter.surface.generation(), surface_generation);
        assert_eq!(presenter.node_id, node_id);
        assert_eq!(presenter.track.id(), track_id);
        assert_eq!(presenter.track.channel_generation(), track_generation);
        assert_eq!(presenter.descriptor.semantic_content_revision, 2);
    }

    /// Connect a presenter to a live fake presenter over a real authenticated session.
    ///
    /// `VIVID_ROOT_SECRET` is process-global, so live tests are serialized on one mutex.
    fn live_presenter(
        fake: &vivid_sdk::testing::TestPresenter,
        viewport: WindowSize,
    ) -> VividPresenter {
        let config = ProducerConfig {
            endpoint_control: Some(fake.endpoint().to_owned()),
            endpoint_bulk: Some(fake.endpoint().to_owned()),
            authentication: vivid_sdk::ProducerAuthentication::root_hex(
                vivid_sdk::testing::ROOT_SECRET_HEX,
            )
            .unwrap(),
            producer_name: "vvrd-test".to_owned(),
            target_profile: vivid_sdk::TERMINAL_SURFACE.to_owned(),
            required_profiles: vec![
                vivid_sdk::LIVE_MEDIA.to_owned(),
                vivid_sdk::TERMINAL_SURFACE.to_owned(),
                vivid_sdk::CORE_CONTROL.to_owned(),
            ],
            optional_profiles: Vec::new(),
            ..ProducerConfig::default()
        };
        let session = Session::connect(config).expect("live session");
        VividPresenter::new(session, viewport, 0, test_descriptor(), 0).expect("live presenter")
    }

    #[test]
    fn resize_and_teardown_destroy_tracks_before_closing_their_transports() {
        let fake = vivid_sdk::testing::TestPresenter::start(8, 6).unwrap();
        let viewport = WindowSize::from_cells(8, 6, 10, 20);
        let resized = WindowSize::from_cells(10, 6, 10, 20);
        let mut presenter = live_presenter(&fake, viewport);

        presenter.resize(resized, true).expect("settled resize");
        presenter.teardown().expect("teardown");

        // Both retired tracks were destroyed on the ordered control connection before their media
        // transports closed. The reverse order makes a relay reject the destroy.
        let destroys = fake.destroys();
        assert_eq!(destroys.len(), 2, "expected two destroys: {destroys:?}");
        for destroy in &destroys {
            assert!(
                !destroy.closed_before_destroy,
                "track {} closed its transport before DESTROY_TRACK: {destroys:?}",
                destroy.track_id
            );
        }

        // Each track opened its own generation-one channel: a settled resize replaces the track
        // rather than advancing the retired track's channel.
        let channels = wait_for(|| {
            let channels = fake.channels();
            (channels.len() == 2).then_some(channels)
        })
        .expect("a resize should establish a second track channel");
        for channel in &channels {
            assert_eq!(channel.channel_generation, 1);
            assert!(channel.media_records >= 1, "channel carried no media");
        }
        assert_ne!(
            channels[0].track_id, channels[1].track_id,
            "the replacement channel reused the retired track's identity"
        );

        // Teardown order: the node leaves the scene, then the track, then the surface.
        let types: Vec<_> = fake
            .observed()
            .iter()
            .map(|observed| observed.record_type)
            .collect();
        let position = |wanted: u16| types.iter().rposition(|value| *value == wanted);
        assert!(position(messages_delete_node()) < position(messages_destroy_track()));
        assert!(position(messages_destroy_track()) < position(messages_destroy_surface()));
    }

    /// A resize is announced before it is reachable: the presenter owns the new target the moment
    /// the window moves, so a placement the reader planned a moment earlier is rejected as stale.
    /// The reader has to follow the target and commit again, not treat the rejection as fatal.
    #[test]
    fn a_commit_that_crosses_a_resize_follows_the_target_instead_of_failing() {
        let fake = vivid_sdk::testing::TestPresenter::start(8, 6).unwrap();
        let mut presenter = live_presenter(&fake, WindowSize::from_cells(8, 6, 10, 20));

        // Mid-drag: only the node placement moves, and the target moved under it.
        let dragged = WindowSize::from_cells(12, 6, 10, 20);
        fake.resize_terminal(12, 6, false).unwrap();
        presenter
            .resize(dragged, false)
            .expect("an unsettled resize must survive the target it was planned against moving");
        assert_eq!(presenter.node_viewport, dragged);

        // Settled: the track is replaced and the node re-placed, again across a moved target.
        let settled = WindowSize::from_cells(14, 8, 10, 20);
        fake.resize_terminal(14, 8, true).unwrap();
        presenter
            .resize(settled, true)
            .expect("a settled resize must survive the target it was planned against moving");
        assert_eq!(presenter.track_viewport, settled);
        assert_eq!(presenter.node_viewport, settled);

        // Following the target is not silent: the reader still learns that it moved.
        assert!(
            presenter
                .signals
                .iter()
                .any(|signal| *signal == PresenterSignal::TargetChanged),
            "the applied target changes were never reported: {:?}",
            presenter.signals
        );
        presenter
            .teardown()
            .expect("teardown after following the target");
    }

    /// A window dragged down to one row leaves a target this reader cannot draw into. That is not a
    /// broken session: the document, the surface, the track, and the node all have to survive it,
    /// and the reader has to pick presentation back up when the window grows again.
    #[test]
    fn a_target_too_small_to_present_is_survived_rather_than_fatal() {
        let fake = vivid_sdk::testing::TestPresenter::start(80, 24).unwrap();
        let viewport = WindowSize::from_cells(80, 24, 10, 20);
        let mut presenter = live_presenter(&fake, viewport);

        fake.resize_terminal(80, 1, true).unwrap();
        let waited = wait_for(|| {
            presenter.take_signal();
            matches!(
                presenter.target_viewport(),
                Ok((TargetViewport::TooSmall { .. }, _))
            )
            .then_some(())
        });
        assert!(
            waited.is_some(),
            "a one-row target was not reported as too small to present"
        );
        assert_eq!(
            presenter.target_viewport().unwrap(),
            (TargetViewport::TooSmall { cols: 80, rows: 1 }, true)
        );

        // The session is still live through the unpresentable target: the node it already owns
        // stays addressable, which is what lets the reader hide it and wait.
        presenter
            .set_visible(false)
            .expect("hiding the node during an unpresentable target must still be accepted");

        fake.resize_terminal(80, 24, true).unwrap();
        let recovered = wait_for(|| {
            presenter.take_signal();
            matches!(
                presenter.target_viewport(),
                Ok((TargetViewport::Presentable(_), _))
            )
            .then_some(())
        });
        assert!(
            recovered.is_some(),
            "the reader never saw the target become presentable again"
        );
        presenter
            .resize(viewport, true)
            .expect("resize after the target became presentable again");
        presenter
            .set_visible(true)
            .expect("the node must be showable again once the target has room");
        presenter
            .teardown()
            .expect("teardown after a too-small target");
    }

    fn messages_delete_node() -> u16 {
        registry::record::DELETE_NODE
    }

    fn messages_destroy_track() -> u16 {
        registry::record::DESTROY_TRACK
    }

    fn messages_destroy_surface() -> u16 {
        registry::record::DESTROY_SURFACE
    }

    #[test]
    fn two_owners_reusing_object_numbers_stay_isolated_through_track_loss() {
        // Two independent owners whose SDK sessions allocate the same numeric surface, node, and
        // track IDs. Only the complete owner identity distinguishes them.
        let first_presenter = vivid_sdk::testing::TestPresenter::start(8, 6).unwrap();
        let second_presenter = vivid_sdk::testing::TestPresenter::start(8, 6).unwrap();
        let viewport = WindowSize::from_cells(8, 6, 10, 20);
        let mut first = live_presenter(&first_presenter, viewport);
        let mut second = live_presenter(&second_presenter, viewport);

        let first_surface = first.surface.id();
        let first_track = first.track.id();
        let second_surface = second.surface.id();
        let second_track = second.track.id();
        let second_node = second.node_id;
        let second_generation = second.surface.generation();
        assert_eq!(
            (first_surface, first_track),
            (second_surface, second_track),
            "the regression requires both owners to reuse the same object numbers"
        );

        let first_identity = first.track.configuration().unwrap();
        first_presenter
            .lose_track(
                first_identity.context_id,
                first_identity.surface_id,
                first_identity.track_id,
            )
            .unwrap();
        // The second owner is told about a loss that reuses its own surface and track numbers under
        // a different context. It must ignore it completely.
        second_presenter
            .lose_track(
                first_identity.context_id + 1,
                first_identity.surface_id,
                first_identity.track_id,
            )
            .unwrap();

        let lost = wait_for_signal(&mut first, |signal| {
            matches!(signal, PresenterSignal::TrackLost(_))
        });
        assert!(lost, "the owning producer never observed its track loss");
        first.recover_track().expect("track recovery");

        // The affected owner kept its logical identity and replaced only the media track.
        assert_eq!(first.surface.id(), first_surface);
        assert_ne!(first.track.id(), first_track);

        // The unaffected owner is byte-for-byte unchanged and still submits media.
        for _ in 0..8 {
            assert!(
                second.take_signal().is_none(),
                "an unrelated owner reacted to another owner's track loss"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(second.surface.id(), second_surface);
        assert_eq!(second.track.id(), second_track);
        assert_eq!(second.node_id, second_node);
        assert_eq!(second.surface.generation(), second_generation);
        let frame = vec![0_u8; viewport.framebuffer_len().unwrap()];
        second
            .show_frame(&frame, None)
            .expect("the unaffected owner's next frame still succeeds");
    }

    #[test]
    fn a_dead_track_transport_becomes_an_actionable_channel_loss() {
        let fake = vivid_sdk::testing::TestPresenter::start(8, 6).unwrap();
        let viewport = WindowSize::from_cells(8, 6, 10, 20);
        let mut presenter = live_presenter(&fake, viewport);
        let track_id = presenter.track.id();
        let surface_id = presenter.surface.id();
        let frame = vec![0_u8; viewport.framebuffer_len().unwrap()];
        presenter.show_frame(&frame, None).expect("first frame");

        // Drop the transport without touching the track: the SDK surfaces this only as a send
        // failure, so the presenter has to turn it into a recoverable signal rather than an error.
        presenter.channel = None;
        let error = presenter.show_frame(&frame, None).unwrap_err();
        assert!(error.to_string().contains("unavailable"));

        presenter
            .recover_channel(registry::error::DEVICE_LOST)
            .expect("channel recovery");
        assert_eq!(
            presenter.track.id(),
            track_id,
            "recovery replaced the track"
        );
        assert_eq!(presenter.surface.id(), surface_id);
        assert_eq!(
            presenter.channel.as_ref().map(TrackChannel::generation),
            Some(vivid_protocol::revision::ChannelGeneration::new(2)),
            "recovery did not advance the channel generation"
        );
        presenter
            .show_frame(&frame, None)
            .expect("the recovered channel accepts a full frame");
    }

    /// Poll a presenter-side observation until it settles, bounded at two seconds.
    fn wait_for<T>(mut observe: impl FnMut() -> Option<T>) -> Option<T> {
        for _ in 0..200 {
            if let Some(value) = observe() {
                return Some(value);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    fn wait_for_signal(
        presenter: &mut VividPresenter,
        predicate: impl Fn(&PresenterSignal) -> bool,
    ) -> bool {
        for _ in 0..200 {
            while let Some(signal) = presenter.take_signal() {
                if predicate(&signal) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn record_signal_for_test(presenter: &mut VividPresenter, event: SessionEvent) {
        record_signal(
            &mut presenter.session,
            &presenter.track,
            &mut presenter.signals,
            event,
        );
    }

    #[test]
    fn track_loss_is_matched_by_complete_owner_identity() {
        let viewport = WindowSize::from_cells(4, 5, 1, 1);
        let mut presenter =
            VividPresenter::new(offline_session(), viewport, 0, test_descriptor(), 0).unwrap();
        let configuration = presenter.track.configuration().unwrap();
        let loss = |context_id: u64, surface_id: u64, track_id: u64| SessionEvent::TrackLost {
            object_id: track_id,
            payload: vec![
                (0, Value::Unsigned(context_id)),
                (1, Value::Unsigned(surface_id)),
                (2, Value::Unsigned(track_id)),
                (3, Value::Unsigned(registry::error::DECODER)),
            ],
        };

        // Another owner reusing our numeric track ID must not be mistaken for our own loss.
        record_signal_for_test(
            &mut presenter,
            loss(
                configuration.context_id + 1,
                configuration.surface_id,
                configuration.track_id,
            ),
        );
        record_signal_for_test(
            &mut presenter,
            loss(
                configuration.context_id,
                configuration.surface_id + 1,
                configuration.track_id,
            ),
        );
        assert!(presenter.signals.is_empty());

        record_signal_for_test(
            &mut presenter,
            loss(
                configuration.context_id,
                configuration.surface_id,
                configuration.track_id,
            ),
        );
        assert!(matches!(
            presenter.signals.pop_front(),
            Some(PresenterSignal::TrackLost(_))
        ));
    }
}
