//! GPU-side mirror of a [`crate::VideoStream`]: a single
//! `wgpu::Texture` per stream, written in-place each time the
//! decode worker hands us a fresh frame. The texture is wrapped in
//! damascene's [`AppTexture`] so it can be handed to `surface()` for
//! compositing — analogous to `rumble-damascene::animated_gpu`'s
//! pattern for animated images.
//!
//! Lifecycle: allocate once on first use (sized to the stream's
//! natural dimensions), upload via [`Self::upload_if_changed`] in
//! the host's `before_paint`, drop alongside the owning
//! `VideoStream`. The texture's id is stable for the lifetime of
//! the `VideoGpu`, so wgpu's bind-group cache stays warm across
//! frames.

use std::sync::Arc;

use damascene_core::surface::AppTexture;

use crate::VideoStream;

/// Per-stream GPU mirror. One `wgpu::Texture` sized to the stream's
/// `(dwidth, dheight)`, refreshed in-place on every new frame.
#[derive(Debug)]
pub struct VideoGpu {
    /// App-owned wgpu texture. `queue.write_texture` writes into
    /// it; damascene composites by sampling.
    texture: Arc<wgpu::Texture>,
    /// Damascene wrapper around `texture`. Cheap to clone (`Arc`-
    /// backed); the underlying id is stable across frames.
    app_texture: AppTexture,
    /// Last frame seq we uploaded. Compared against
    /// `VideoStream::frame_seq()` to skip redundant uploads when
    /// playback is paused or the worker hasn't produced a new
    /// frame yet.
    last_seq: Option<u64>,
    /// Cached pixel size. Saves an `AppTexture::size_px` round trip
    /// every frame.
    size: (u32, u32),
}

impl VideoGpu {
    /// Allocate a fresh `Rgba8UnormSrgb` texture sized for `stream`.
    /// All frames in a stream share the same dimensions (the
    /// decoder enforces this — mid-stream resolution changes are
    /// not handled here), so we can size the allocation up-front
    /// from `stream.dimensions()`.
    pub fn allocate(device: &wgpu::Device, stream: &VideoStream) -> Self {
        let (width, height) = stream.dimensions();
        let texture = Arc::new(device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rumble-video::frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        }));
        let app_texture = damascene_wgpu::app_texture(texture.clone());
        Self {
            texture,
            app_texture,
            last_seq: None,
            size: (width, height),
        }
    }

    /// AppTexture handle to pass into `surface(...)`.
    pub fn app_texture(&self) -> &AppTexture {
        &self.app_texture
    }

    /// Pixel size of the underlying texture.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Upload the latest decoded frame from `stream` into the
    /// texture if a newer one is available. Returns `true` when an
    /// upload actually happened. Cheap when paused / between
    /// frames: a single atomic load, then bail.
    pub fn upload_if_changed(&mut self, queue: &wgpu::Queue, stream: &VideoStream) -> bool {
        let seq = stream.frame_seq();
        if self.last_seq == Some(seq) {
            return false;
        }
        // Hold the stream's mailbox lock only as long as the
        // texture write takes. queue.write_texture is a memcpy +
        // command-buffer enqueue — sub-millisecond — so the
        // worker rarely contends.
        stream.with_latest_frame(|frame| {
            debug_assert_eq!(
                (frame.width, frame.height),
                self.size,
                "video frame dims drifted from texture allocation",
            );
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &frame.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(frame.stride as u32),
                    rows_per_image: Some(frame.height),
                },
                wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
            );
        });
        self.last_seq = Some(seq);
        true
    }
}
