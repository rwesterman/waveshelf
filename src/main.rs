mod app;
mod audio;
mod format;
mod library;
mod waveform;
mod waveform_view;

fn main() -> eframe::Result {
    ffmpeg_next::init().expect("initialising FFmpeg");
    ffmpeg_next::log::set_level(ffmpeg_next::log::Level::Error);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Waveshelf")
            .with_inner_size([1200.0, 760.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Waveshelf",
        options,
        Box::new(|cc| Ok(Box::new(app::WaveshelfApp::new(cc)))),
    )
}
