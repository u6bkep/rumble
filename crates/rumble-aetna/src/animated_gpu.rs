//! GPU-side mirror for animated chat previews.
//!
//! Animated entries (GIF, animated WebP, APNG) decode to a sequence of
//! CPU [`aetna_core::Image`] frames. To play one through aetna's
//! `surface()` widget we keep a single `wgpu::Texture` per active
//! entry and write the current frame's pixels in-place when the
//! playhead advances. Aetna only samples it — no per-frame upload
//! through aetna's image content-hash cache.
//!
//! Lifecycle: an entry is allocated on first paint while a matching
//! [`crate::chat::GifPlayback`] is alive, and dropped together with
//! that playback.

use std::{sync::Arc, time::Duration};

use aetna_core::{prelude::Image, surface::AppTexture};

/// Per-entry GPU mirror of a [`crate::chat::CachedImage::Animated`].
#[derive(Debug)]
pub struct AnimatedGpu {
    /// App-owned wgpu texture. We `queue.write_texture` into this on
    /// frame advance; aetna composites by sampling.
    texture: Arc<wgpu::Texture>,
    /// Aetna wrapper around `texture`, handed to `surface(...)` in the
    /// El tree. Cheap to clone (`Arc`-backed); the underlying id is
    /// stable across frames so the wgpu backend's bind-group cache
    /// stays warm.
    app_texture: AppTexture,
    /// Index of the last frame uploaded. Skip the write when the
    /// playhead hasn't advanced — paused animations cost zero
    /// per-frame GPU work.
    last_uploaded: Option<usize>,
    /// Cached pixel size of `texture`. Saves an Arc round-trip
    /// through `AppTexture::size_px()` at every render.
    size: (u32, u32),
}

impl AnimatedGpu {
    /// Allocate a fresh `Rgba8UnormSrgb` texture sized for `frame`.
    /// All frames in an animated entry share the same dimensions
    /// (the decoder enforces this), so one frame is enough to size
    /// the allocation.
    pub fn allocate(device: &wgpu::Device, frame: &Image) -> Self {
        let size = (frame.width(), frame.height());
        let texture = Arc::new(device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rumble-aetna::animated_preview"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        }));
        let app_texture = aetna_wgpu::app_texture(texture.clone());
        Self {
            texture,
            app_texture,
            last_uploaded: None,
            size,
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

    /// Upload `frames[idx]` if it isn't already on the GPU. Returns
    /// `true` when an upload actually happened.
    pub fn upload_frame(&mut self, queue: &wgpu::Queue, frames: &[(Image, Duration)], idx: usize) -> bool {
        if self.last_uploaded == Some(idx) {
            return false;
        }
        let img = &frames[idx].0;
        debug_assert_eq!(
            (img.width(), img.height()),
            self.size,
            "animated frame dims drifted from texture allocation",
        );
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            img.pixels(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * self.size.0),
                rows_per_image: Some(self.size.1),
            },
            wgpu::Extent3d {
                width: self.size.0,
                height: self.size.1,
                depth_or_array_layers: 1,
            },
        );
        self.last_uploaded = Some(idx);
        true
    }
}
