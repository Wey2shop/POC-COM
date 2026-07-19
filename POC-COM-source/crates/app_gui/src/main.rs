//! POC-COM GUI entry point: Mail / Social modes over the same
//! voice-channel-only audio path. No volume/gain controls and no audio
//! passthrough anywhere in this app -- see `audio_io`'s crate docs for why
//! that's structural, not just a UI choice.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod board_ui;
mod listen;
mod mail_ui;
mod maidenhead;
mod mh_map;
mod pipeline;
mod settings;
mod theme;
mod vad;
mod waterfall;
mod wav;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1180.0, 700.0]).with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("POC-COM", options, Box::new(|_cc| Ok(Box::new(app::PocComApp::default()))))
}
