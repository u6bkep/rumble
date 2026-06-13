//! GPU-side mirror of a [`crate::VideoStream`]: a damascene
//! [`StreamingTexture`] per stream, written in-place each time the
//! decode worker hands us a fresh frame. The texture is exposed as an
//! [`AppTexture`] so it can be handed to `surface()` for compositing —
//! analogous to `rumble-damascene`'s animated-image pattern.
//!
//! `StreamingTexture` (damascene #95) owns the create / write_texture /
//! `app_texture` lifecycle this module used to hand-roll: it allocates
//! the `TEXTURE_BINDING | COPY_DST` texture, uploads tightly-packed
//! frames, and hands out an `AppTexture` handle whose identity is
//! stable across writes (so the wgpu bind-group cache stays warm) and
//! changes only when a size change forces a new GPU texture. Our frame
//! dimensions are fixed for the stream's lifetime, so that resize path
//! never fires here.
//!
//! Lifecycle: allocate once on first use (sized to the stream's
//! natural dimensions), upload via [`Self::upload_if_changed`] in the
//! host's `before_paint`, drop alongside the owning `VideoStream`. We
//! keep the stream's monotonic frame-seq dedup on top of
//! `StreamingTexture::write_frame` — a single atomic load is cheaper
//! than the content hash `write_frame_if_changed` would compute, and
//! for live video every frame differs anyway.

use damascene_core::surface::AppTexture;
use damascene_wgpu::StreamingTexture;

use crate::VideoStream;

/// Per-stream GPU mirror. Wraps a [`StreamingTexture`] sized to the
/// stream's `(dwidth, dheight)`, refreshed in-place on every new frame.
pub struct VideoGpu {
    /// Owned streaming texture. `write_frame` uploads into it;
    /// damascene composites by sampling its `AppTexture`.
    tex: StreamingTexture,
    /// Device clone retained for `StreamingTexture::write_frame`, which
    /// takes a device so it can recreate the texture on a size change.
    /// Dimensions are fixed here, so it is only ever used as the
    /// upload's device handle. `wgpu::Device` is `Arc`-backed, so the
    /// clone is cheap.
    device: wgpu::Device,
    /// Last frame seq we uploaded. Compared against
    /// `VideoStream::frame_seq()` to skip redundant uploads when
    /// playback is paused or the worker hasn't produced a new frame
    /// yet.
    last_seq: Option<u64>,
}

impl VideoGpu {
    /// Allocate a fresh `Rgba8UnormSrgb` streaming texture sized for
    /// `stream`. All frames in a stream share the same dimensions (the
    /// decoder enforces this — mid-stream resolution changes are not
    /// handled here), so we can size the allocation up-front from
    /// `stream.dimensions()`.
    pub fn allocate(device: &wgpu::Device, stream: &VideoStream) -> Self {
        let (width, height) = stream.dimensions();
        let tex = StreamingTexture::new(device, wgpu::TextureFormat::Rgba8UnormSrgb, width, height);
        Self {
            tex,
            device: device.clone(),
            last_seq: None,
        }
    }

    /// AppTexture handle to pass into `surface(...)`. Cheap clone;
    /// identity is stable across `upload_if_changed` calls.
    pub fn app_texture(&self) -> AppTexture {
        self.tex.app_texture()
    }

    /// Pixel size of the underlying texture.
    pub fn size(&self) -> (u32, u32) {
        self.tex.size()
    }

    /// Upload the latest decoded frame from `stream` into the texture
    /// if a newer one is available. Returns `true` when an upload
    /// actually happened. Cheap when paused / between frames: a single
    /// atomic load, then bail.
    pub fn upload_if_changed(&mut self, queue: &wgpu::Queue, stream: &VideoStream) -> bool {
        let seq = stream.frame_seq();
        if self.last_seq == Some(seq) {
            return false;
        }
        // Hold the stream's mailbox lock only as long as the texture
        // write takes. queue.write_texture is a memcpy + command-buffer
        // enqueue — sub-millisecond — so the worker rarely contends.
        let device = &self.device;
        let tex = &mut self.tex;
        stream.with_latest_frame(|frame| {
            debug_assert_eq!(
                (frame.width, frame.height),
                tex.size(),
                "video frame dims drifted from texture allocation",
            );
            // `frame.data` is tightly packed (`stride == width * 4`),
            // which is exactly what `StreamingTexture::write_frame`
            // expects.
            tex.write_frame(device, queue, frame.width, frame.height, &frame.data);
        });
        self.last_seq = Some(seq);
        true
    }
}
