//! Phase-2 integration test: open a video file, run libmpv on its
//! own decode worker, mirror frames into a wgpu texture, composite
//! through aetna's `surface()` widget into a real winit window.
//!
//! Run with `cargo run -p rumble-video --example window_play -- input.mp4`.
//! The window opens at the video's natural aspect, plays in a loop
//! with audio routed through libmpv's default output, and exits
//! when you close the window.

use std::{path::PathBuf, time::Duration};

use aetna_core::{App, BuildCx, Rect, prelude::*};
use aetna_winit_wgpu::{HostConfig, WinitWgpuApp, run_host_app_with_config};
use rumble_video::{VideoGpu, VideoStream};

struct VideoApp {
    stream: VideoStream,
    /// Stashed in `gpu_setup` and used to lazily allocate
    /// `video_gpu` once we have a wgpu device. `Option` because
    /// `gpu_setup` runs after construction.
    device: Option<wgpu::Device>,
    video_gpu: Option<VideoGpu>,
}

impl App for VideoApp {
    fn build(&self, _cx: &BuildCx) -> El {
        // Keep the El tree minimal: a single surface() centred in
        // the window with `ImageFit::Contain` letterboxing into
        // whatever the user resizes the window to. The 33ms
        // `redraw_within` keeps the host loop ticking while the
        // worker is producing frames; it goes away naturally if
        // the window is hidden (off-screen surfaces drop out of
        // aetna's redraw aggregator).
        match self.video_gpu.as_ref() {
            Some(gpu) => surface(gpu.app_texture().clone())
                .surface_alpha(SurfaceAlpha::Opaque)
                .surface_fit(ImageFit::Contain)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0))
                .redraw_within(Duration::from_millis(33)),
            None => text("initialising…").width(Size::Fill(1.0)).height(Size::Fill(1.0)),
        }
    }
}

impl WinitWgpuApp for VideoApp {
    fn gpu_setup(&mut self, device: &wgpu::Device, _queue: &wgpu::Queue) {
        // Allocate the GPU mirror up-front: dimensions are known
        // already (load_file blocked until VIDEO_RECONFIG before
        // we got here).
        self.video_gpu = Some(VideoGpu::allocate(device, &self.stream));
        self.device = Some(device.clone());
    }

    fn before_paint(&mut self, queue: &wgpu::Queue) {
        if let Some(gpu) = self.video_gpu.as_mut() {
            gpu.upload_if_changed(queue, &self.stream);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .init();

    let input = PathBuf::from(std::env::args().nth(1).ok_or("usage: window_play <input.mp4>")?);

    // Open the stream looped so the window stays alive past
    // end-of-file. The decode worker fires up immediately and
    // starts producing frames in the background.
    let stream = VideoStream::open(&input, true)?;
    let (w, h) = stream.dimensions();
    tracing::info!(width = w, height = h, "playing {}", input.display());

    let app = VideoApp {
        stream,
        device: None,
        video_gpu: None,
    };

    // Open at the video's natural pixel size so first paint
    // doesn't trigger a layout recalc. User can resize freely
    // afterwards — surface_fit(Contain) handles the letterbox.
    let viewport = Rect::new(0.0, 0.0, w as f32, h as f32);
    let host_config = HostConfig::default();
    run_host_app_with_config("rumble-video::window_play", viewport, app, host_config)?;
    Ok(())
}
