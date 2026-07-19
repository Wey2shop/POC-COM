//! GUI wrapper around `channel_probe`: device dropdowns (same idea as the
//! real app's) plus a live waterfall and level meter, so you can *watch*
//! whether anything reaches the RX device in real time -- e.g. while
//! manually keying PTT -- instead of waiting on a blind timed capture and
//! finding out only afterward that nothing came through.
mod waterfall;

use std::time::{Duration, Instant};
use waterfall::Waterfall;

const BAND_LOW_HZ: f32 = 200.0;
const BAND_HIGH_HZ: f32 = 3_500.0;
const METER_WINDOW_S: f32 = 0.1;
const FEED_WINDOW_SAMPLES: usize = 2048 + 200 * 6;
const POLL_INTERVAL: Duration = Duration::from_millis(15);

enum TxState {
    Idle,
    Countdown { started: Instant },
    Playing { handle: audio_io::TxHandle },
}

struct ChannelProbeApp {
    input_devices: Vec<audio_io::DeviceInfo>,
    output_devices: Vec<audio_io::DeviceInfo>,
    selected_input: Option<String>,
    selected_output: Option<String>,
    device_error: Option<String>,

    rx: Option<audio_io::RxHandle>,
    tx: TxState,
    last_feed: Instant,
    level_rms: f32,
    level_peak: f32,

    waterfall: Waterfall,
    status: String,
    last_report: Option<channel_probe::AnalysisReport>,
    last_save_path: Option<String>,
}

impl Default for ChannelProbeApp {
    fn default() -> Self {
        let (input_devices, output_devices, device_error) = match (audio_io::list_input_devices(), audio_io::list_output_devices()) {
            (Ok(inputs), Ok(outputs)) => (inputs, outputs, None),
            (in_res, out_res) => {
                let err = in_res.err().or(out_res.err()).map(|e| e.to_string());
                (Vec::new(), Vec::new(), err)
            }
        };
        let selected_input = input_devices.first().map(|d| d.name.clone());
        let selected_output = output_devices.first().map(|d| d.name.clone());
        Self {
            input_devices,
            output_devices,
            selected_input,
            selected_output,
            device_error,
            rx: None,
            tx: TxState::Idle,
            last_feed: Instant::now(),
            level_rms: 0.0,
            level_peak: 0.0,
            waterfall: Waterfall::new(BAND_LOW_HZ, BAND_HIGH_HZ),
            status: "Idle. Pick devices, then \"Start Listening\".".to_string(),
            last_report: None,
            last_save_path: None,
        }
    }
}

impl ChannelProbeApp {
    fn refresh_devices(&mut self) {
        match (audio_io::list_input_devices(), audio_io::list_output_devices()) {
            (Ok(inputs), Ok(outputs)) => {
                self.input_devices = inputs;
                self.output_devices = outputs;
                self.device_error = None;
                if self.selected_input.as_ref().map(|s| !self.input_devices.iter().any(|d| &d.name == s)).unwrap_or(true) {
                    self.selected_input = self.input_devices.first().map(|d| d.name.clone());
                }
                if self.selected_output.as_ref().map(|s| !self.output_devices.iter().any(|d| &d.name == s)).unwrap_or(true) {
                    self.selected_output = self.output_devices.first().map(|d| d.name.clone());
                }
            }
            (in_res, out_res) => {
                self.device_error = in_res.err().or(out_res.err()).map(|e| e.to_string());
            }
        }
    }

    fn start_listening(&mut self) {
        let Some(device) = self.selected_input.clone() else {
            self.status = "No input device selected.".to_string();
            return;
        };
        match audio_io::start_reception(&device) {
            Ok(handle) => {
                self.rx = Some(handle);
                self.status = format!("Listening on \"{device}\"...");
            }
            Err(e) => self.status = format!("Failed to start capture: {e}"),
        }
    }

    fn stop_listening_and_analyze(&mut self) {
        let Some(handle) = self.rx.take() else {
            self.status = "Not currently listening.".to_string();
            return;
        };
        let samples = handle.finish(channel_probe::SAMPLE_RATE);
        let path = "channel_probe_gui_capture.wav";
        match channel_probe::wav::write_wav_f32(std::path::Path::new(path), channel_probe::SAMPLE_RATE, &samples) {
            Ok(()) => self.last_save_path = Some(path.to_string()),
            Err(e) => self.status = format!("Captured, but failed to save {path}: {e}"),
        }
        let report = channel_probe::analyze_samples(&samples);
        self.status = if report.looks_reliable() {
            format!("Analyzed {:.2}s capture -- start marker found cleanly.", samples.len() as f32 / channel_probe::SAMPLE_RATE as f32)
        } else {
            "Analyzed capture -- start marker was NOT found reliably (see warning below). Check PTT/levels/routing.".to_string()
        };
        self.last_report = Some(report);
    }

    fn start_play_countdown(&mut self) {
        if self.selected_output.is_none() {
            self.status = "No output device selected.".to_string();
            return;
        }
        self.tx = TxState::Countdown { started: Instant::now() };
    }
}

impl eframe::App for ChannelProbeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Advance TX countdown -> playback.
        if let TxState::Countdown { started } = &self.tx {
            let elapsed = started.elapsed().as_secs_f32();
            if elapsed >= 3.0 {
                let device = self.selected_output.clone().unwrap();
                let (wave, _segments) = channel_probe::build_signal();
                match audio_io::start_transmission(&device, wave, channel_probe::SAMPLE_RATE) {
                    Ok(handle) => {
                        self.status = "Playing diagnostic signal...".to_string();
                        self.tx = TxState::Playing { handle };
                    }
                    Err(e) => {
                        self.status = format!("Failed to start playback: {e}");
                        self.tx = TxState::Idle;
                    }
                }
            }
        }
        if let TxState::Playing { handle } = &self.tx {
            if handle.is_finished() {
                self.status = "Playback finished.".to_string();
                self.tx = TxState::Idle;
            }
        }

        // Pull recent RX audio into the waterfall + level meter.
        if let Some(rx) = &self.rx {
            if self.last_feed.elapsed() >= POLL_INTERVAL {
                self.last_feed = Instant::now();
                let rate = rx.device_rate();
                let feed = rx.tail(FEED_WINDOW_SAMPLES);
                self.waterfall.push_samples(&feed, rate);

                let meter_n = ((rate as f32 * METER_WINDOW_S) as usize).max(1);
                let meter_tail = rx.tail(meter_n);
                self.level_rms = channel_probe::rms(&meter_tail);
                self.level_peak = meter_tail.iter().fold(0.0f32, |acc, &x| acc.max(x.abs()));
            }
        }

        if self.rx.is_some() || matches!(self.tx, TxState::Countdown { .. } | TxState::Playing { .. }) {
            ctx.request_repaint();
        }

        egui::TopBottomPanel::top("devices").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Input (RX):");
                egui::ComboBox::from_id_salt("input_device")
                    .selected_text(self.selected_input.clone().unwrap_or_else(|| "(none)".to_string()))
                    .show_ui(ui, |ui| {
                        for d in self.input_devices.clone() {
                            ui.selectable_value(&mut self.selected_input, Some(d.name.clone()), d.name);
                        }
                    });
                ui.add_space(20.0);
                ui.label("Output (TX):");
                egui::ComboBox::from_id_salt("output_device")
                    .selected_text(self.selected_output.clone().unwrap_or_else(|| "(none)".to_string()))
                    .show_ui(ui, |ui| {
                        for d in self.output_devices.clone() {
                            ui.selectable_value(&mut self.selected_output, Some(d.name.clone()), d.name);
                        }
                    });
                ui.add_space(20.0);
                if ui.button("Refresh devices").clicked() {
                    self.refresh_devices();
                }
            });
            if let Some(err) = &self.device_error {
                ui.colored_label(egui::Color32::from_rgb(200, 90, 60), format!("Device enumeration error: {err}"));
            }
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let listening = self.rx.is_some();
                if listening {
                    if ui.button("Stop Listening + Analyze").clicked() {
                        self.stop_listening_and_analyze();
                    }
                } else if ui.button("Start Listening").clicked() {
                    self.start_listening();
                }

                ui.add_space(12.0);
                let tx_busy = !matches!(self.tx, TxState::Idle);
                ui.add_enabled_ui(!tx_busy, |ui| {
                    if ui.button("Play Diagnostic Signal (3s countdown)").clicked() {
                        self.start_play_countdown();
                    }
                });

                ui.add_space(12.0);
                ui.label(&self.status);
            });

            match &self.tx {
                TxState::Countdown { started } => {
                    let remaining = (3.0 - started.elapsed().as_secs_f32()).max(0.0);
                    ui.label(egui::RichText::new(format!("Key up PTT now -- playback in {remaining:.1}s")).strong().color(egui::Color32::from_rgb(220, 140, 40)));
                }
                TxState::Playing { .. } => {
                    ui.label(egui::RichText::new("Transmitting...").strong().color(egui::Color32::from_rgb(60, 170, 100)));
                }
                TxState::Idle => {}
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Level (last 100ms):");
                let bar_frac = (self.level_rms * 8.0).clamp(0.0, 1.0);
                ui.add(egui::ProgressBar::new(bar_frac).desired_width(220.0).text(format!("RMS {:.4}", self.level_rms)));
                ui.add_space(12.0);
                ui.label(format!("peak {:.4}", self.level_peak));
            });
            ui.add_space(8.0);
        });

        if let Some(report) = &self.last_report {
            egui::SidePanel::right("report").min_width(340.0).show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("Last analysis");
                if let Some(path) = &self.last_save_path {
                    ui.label(format!("saved: {path}"));
                }
                if !report.looks_reliable() {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 90, 60),
                        "WARNING: start marker correlation is very low relative to its own energy -- it probably wasn't found. Everything below is likely unreliable.",
                    );
                }
                ui.label(format!("start marker: sample {} (score {:.4}, ref norm {:.2})", report.start_offset, report.start_score, report.marker_norm));
                ui.label(format!(
                    "end marker: sample {} (expected {}, score {:.4}) -- drift {} samples ({:.1} ppm)",
                    report.end_offset, report.expected_end_offset, report.end_score, report.drift_samples, report.drift_ppm
                ));
                ui.label(format!("noise floor (RMS): {:.6}", report.noise_floor));
                ui.label(format!("overall peak: {:.5}", report.overall_peak));
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for tone in &report.tones {
                        ui.monospace(format!("{:>6.0} Hz   ratio {:>7.1} dB   rx_mag {:>10.4}", tone.freq, tone.ratio_db, tone.rx_mag));
                    }
                });
                ui.add_space(6.0);
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(egui::RichText::new("Live spectrum (RX)").weak().small());
            ui.add_space(4.0);
            self.waterfall.content(ui);
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 680.0]).with_min_inner_size([700.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("channel_probe", options, Box::new(|_cc| Ok(Box::new(ChannelProbeApp::default()))))
}
