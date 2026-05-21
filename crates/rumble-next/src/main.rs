use eframe::{NativeOptions, egui};
use rumble_next::App;

fn main() -> eframe::Result<()> {
    // Set up tracing so rumble-client debug output is visible in the terminal.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,rumble_next=info,rumble_client=info")),
        )
        .init();

    let options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_title("rumble-next"),
        ..Default::default()
    };
    eframe::run_native(
        "rumble-next",
        options,
        Box::new(|cc| {
            App::new(cc)
                .map(|a| Box::new(a) as Box<dyn eframe::App>)
                .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
        }),
    )
}
