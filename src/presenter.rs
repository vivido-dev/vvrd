use std::io;

use vivid_protocol::{
    messages::{self, SceneNodeConfig},
    wire::ConnectionKind,
};
use vivid_sdk::{MediaChannel, ProducerSession, SourceEvent, SourceHandle};

use crate::geometry::WindowSize;

pub trait Presenter {
    fn show_frame(&mut self, rgba: &[u8]) -> io::Result<u64>;
    fn set_visible(&mut self, visible: bool) -> io::Result<()>;
    fn resize(&mut self, viewport: WindowSize) -> io::Result<()>;
    fn recover_source(&mut self) -> io::Result<()>;
    fn take_source_event(&mut self) -> Option<SourceEvent>;
    fn teardown(&mut self) -> io::Result<()>;
}

pub struct VividPresenter {
    session: ProducerSession,
    source: Option<SourceHandle>,
    channel: Option<MediaChannel>,
    source_id: u64,
    node_id: u64,
    viewport: WindowSize,
    visible: bool,
    epoch: u32,
    frame_id: u64,
    torn_down: bool,
}

impl VividPresenter {
    pub fn new(mut session: ProducerSession, viewport: WindowSize) -> io::Result<Self> {
        let source_id = session.allocate_id()?;
        let node_id = session.allocate_id()?;
        let source = session.create_raster_source(
            source_id,
            viewport.page_area_width_px(),
            viewport.page_area_height_px(),
        )?;
        let channel = session.open_media_channel(&source, ConnectionKind::Raster)?;
        session.create_scene_node(&scene_config(
            session.root_context_id(),
            node_id,
            source_id,
            viewport,
            true,
        ))?;

        Ok(Self {
            session,
            source: Some(source),
            channel: Some(channel),
            source_id,
            node_id,
            viewport,
            visible: true,
            epoch: 1,
            frame_id: 0,
            torn_down: false,
        })
    }

    fn replace_source(&mut self, viewport: WindowSize) -> io::Result<()> {
        let old_source_id = self.source_id;
        let source_id = self.session.allocate_id()?;
        let source = self.session.create_raster_source(
            source_id,
            viewport.page_area_width_px(),
            viewport.page_area_height_px(),
        )?;
        let channel = match self
            .session
            .open_media_channel(&source, ConnectionKind::Raster)
        {
            Ok(channel) => channel,
            Err(error) => {
                let _ = self.session.destroy_source(source_id);
                return Err(error);
            }
        };

        if let Err(error) = self.session.update_scene_node(&scene_config(
            self.session.root_context_id(),
            self.node_id,
            source_id,
            viewport,
            self.visible,
        )) {
            let _ = self.session.destroy_source(source_id);
            return Err(error);
        }

        self.source = Some(source);
        self.channel = Some(channel);
        self.source_id = source_id;
        self.viewport = viewport;
        self.epoch = self.epoch.checked_add(1).unwrap_or(1);
        self.frame_id = 0;
        self.session.destroy_source(old_source_id)
    }

    fn expected_frame_len(&self) -> io::Result<usize> {
        self.viewport
            .framebuffer_len()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
    }
}

impl Presenter for VividPresenter {
    fn show_frame(&mut self, rgba: &[u8]) -> io::Result<u64> {
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
        let source = self
            .source
            .as_mut()
            .ok_or_else(|| io::Error::other("raster source is unavailable"))?;
        let channel = self
            .channel
            .as_mut()
            .ok_or_else(|| io::Error::other("raster channel is unavailable"))?;
        self.session.wait_until_visible(source)?;
        self.session.send_raster_frame(
            source,
            channel,
            self.epoch,
            self.frame_id,
            (
                self.viewport.page_area_width_px(),
                self.viewport.page_area_height_px(),
            ),
            rgba,
        )?;
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
            self.viewport,
            visible,
        ))?;
        self.visible = visible;
        Ok(())
    }

    fn resize(&mut self, viewport: WindowSize) -> io::Result<()> {
        if viewport == self.viewport {
            return Ok(());
        }
        self.replace_source(viewport)
    }

    fn recover_source(&mut self) -> io::Result<()> {
        self.replace_source(self.viewport)
    }

    fn take_source_event(&mut self) -> Option<SourceEvent> {
        self.source.as_mut().and_then(SourceHandle::take_event)
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
        self.channel.take();
        self.source.take();
        if let Err(error) = self.session.destroy_source(self.source_id)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = self.session.goodbye()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }
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
    use super::*;

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
}
