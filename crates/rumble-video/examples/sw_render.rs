//! Phase-1 smoke test: open a video file, decode the first frame via
//! libmpv's software renderer, and write it to a PNG.
//!
//! Run with `cargo run -p rumble-video --example sw_render -- input.mp4 out.png`.
//! Proves the link, the SW render API contract, and that our buffer
//! layout (RGBA with alpha forced to 0xff) round-trips through the
//! `image` crate without channel-swap surprises.

use std::{path::PathBuf, time::Duration};

use rumble_video::MpvPlayer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let input = PathBuf::from(args.next().ok_or("usage: sw_render <input> <output.png>")?);
    let output = PathBuf::from(args.next().ok_or("usage: sw_render <input> <output.png>")?);

    let player = MpvPlayer::new()?;
    // Smoke test wants software-only behaviour and no audio so we
    // can run headless on CI without an audio device or GPU.
    player.set_option_string("hwdec", "no")?;
    player.set_option_string("audio", "no")?;
    // Pause at the first frame so the decoder doesn't race ahead.
    player.set_option_string("pause", "yes")?;

    player.load_file(&input)?;
    let (w, h) = player.dimensions()?;
    tracing::info!(width = w, height = h, "loaded {}", input.display());

    let frame_ready = player.wait_for_frame(Duration::from_secs(5))?;
    if !frame_ready {
        return Err("timed out waiting for first frame".into());
    }

    let stride = (w as usize) * 4;
    let mut buf = vec![0u8; stride * h as usize];
    player.render_sw(&mut buf, w, h, stride)?;

    image::save_buffer(&output, &buf, w, h, image::ColorType::Rgba8)?;
    tracing::info!("wrote {} ({}x{})", output.display(), w, h);
    Ok(())
}
