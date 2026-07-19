//! The shared listening session: one continuously-running receive path
//! used regardless of which mode's tab is active. Start/Stop Listening,
//! Load WAV, and Save Last Raw Capture all live here now rather than being
//! duplicated per mode -- there's only one input device, so there was
//! never really two independent listening sessions, just two copies of
//! the same code. Decoded results are classified by
//! `pipeline::decode_any_reception`'s marker-word auto-detection and
//! handed back to `app.rs` (via `drain_pending`) to route into whichever
//! mode's list they belong to (`mail_ui::InboxState::push_message` /
//! `board_ui::FeedState::push_post`) -- this module doesn't know or care
//! which mode is currently on screen.

use crate::pipeline::{decode_any_reception, ReceivedPayload};
use crate::theme::{self, ACCENT, ERROR};
use crate::vad::{VadEvent, VadTracker};
use crate::wav;
use crate::waterfall::WaterfallWindow;
use std::sync::mpsc;
use std::time::Instant;

pub enum ReceivePhase {
    Idle,
    Listening { rx: audio_io::RxHandle, started: Instant, vad: VadTracker, last_poll: Instant },
}

pub struct ListenState {
    pub phase: ReceivePhase,
    pending: Vec<mpsc::Receiver<Result<ReceivedPayload, String>>>,
    error: Option<String>,
    last_raw_capture: Option<Vec<f32>>,
}

impl Default for ListenState {
    fn default() -> Self {
        Self { phase: ReceivePhase::Idle, pending: Vec::new(), error: None, last_raw_capture: None }
    }
}

impl ListenState {
    fn spawn_decode(&mut self, samples: Vec<f32>) {
        self.last_raw_capture = Some(samples.clone());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(decode_any_reception(&samples));
        });
        self.pending.push(rx);
    }
}

/// Drains completed decodes: failures surface inline as `state.error`
/// (there's no mode to attribute a "no usable signal found" to before it's
/// decoded), successes are returned for `app.rs` to route into the right
/// mode's list. Call this before `listen_status_ui` each frame so the
/// pending-decode count it shows reflects this frame's drain.
pub fn drain_pending(state: &mut ListenState) -> Vec<ReceivedPayload> {
    let mut results = Vec::new();
    state.pending.retain_mut(|rx| match rx.try_recv() {
        Ok(Ok(payload)) => {
            results.push(payload);
            false
        }
        Ok(Err(e)) => {
            state.error = Some(e);
            false
        }
        Err(mpsc::TryRecvError::Empty) => true,
        Err(mpsc::TryRecvError::Disconnected) => false,
    });
    results
}

pub fn listen_status_ui(ui: &mut egui::Ui, ctx: &egui::Context, state: &mut ListenState, input_device: Option<&str>, waterfall: &mut WaterfallWindow) {
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            match &state.phase {
                ReceivePhase::Idle => {
                    if ui.add_enabled(input_device.is_some(), egui::Button::new("🎧 Start Listening")).clicked() {
                        match audio_io::start_reception(input_device.unwrap_or_default()) {
                            Ok(rx) => {
                                let vad = VadTracker::new(rx.device_rate());
                                state.phase = ReceivePhase::Listening { rx, started: Instant::now(), vad, last_poll: Instant::now() };
                                state.error = None;
                            }
                            Err(e) => state.error = Some(e.to_string()),
                        }
                    }
                    if ui.button("📂 Load WAV...").clicked() {
                        if let Some(path) = rfd::FileDialog::new().add_filter("WAV audio", &["wav"]).pick_file() {
                            match wav::read_wav_f32(&path) {
                                Ok((rate, samples)) => {
                                    let resampled = audio_io::resample(&samples, rate, 48_000);
                                    state.spawn_decode(resampled);
                                }
                                Err(e) => state.error = Some(e),
                            }
                        }
                    }
                    if ui.add_enabled(state.last_raw_capture.is_some(), egui::Button::new("💾 Save Last Raw Capture...")).clicked() {
                        if let Some(samples) = &state.last_raw_capture {
                            if let Some(path) =
                                rfd::FileDialog::new().set_file_name("poc_com_raw_capture.wav").add_filter("WAV audio", &["wav"]).save_file()
                            {
                                if let Err(e) = wav::write_wav_f32(&path, 48_000, samples) {
                                    state.error = Some(format!("couldn't save raw capture: {e}"));
                                }
                            }
                        }
                    }
                }
                ReceivePhase::Listening { .. } => {
                    ui.add_enabled(false, egui::Button::new("🎧 Start Listening"));
                }
            }
        });

        if let Some(err) = &state.error {
            ui.colored_label(ERROR, err);
        }

        ui.add_space(6.0);

        match &mut state.phase {
            ReceivePhase::Idle => {
                theme::status_dot(ui, egui::Color32::GRAY, "Not listening.");
            }
            ReceivePhase::Listening { rx, started, vad, last_poll } => {
                let elapsed = started.elapsed().as_secs_f32();
                theme::status_dot(ui, ACCENT, vad.status_label());
                ui.label(format!(
                    "Elapsed: {elapsed:.1}s -- {} samples captured -- {} pending decode(s)",
                    rx.samples_captured(),
                    state.pending.len()
                ));

                let level_tail = rx.tail(4800);
                let level = if level_tail.is_empty() {
                    0.0
                } else {
                    (level_tail.iter().map(|x| x * x).sum::<f32>() / level_tail.len() as f32).sqrt()
                };
                ui.add(egui::ProgressBar::new((level * 4.0).min(1.0)).text("input level"));

                if waterfall.wants_feed() {
                    waterfall.feed(&rx.tail(WaterfallWindow::FEED_WINDOW_SAMPLES), rx.device_rate());
                }

                if last_poll.elapsed() >= VadTracker::poll_interval() {
                    *last_poll = Instant::now();
                    if let VadEvent::SegmentReady { start_idx, end_idx } = vad.poll(rx) {
                        if end_idx > start_idx {
                            let device_rate = rx.device_rate();
                            let full = rx.snapshot();
                            let segment = &full[start_idx.min(full.len())..end_idx.min(full.len())];
                            let resampled = audio_io::resample(segment, device_rate, 48_000);
                            state.spawn_decode(resampled);
                        }
                    }
                }
                ui.add_space(4.0);
                if ui.button("Stop Listening").clicked() {
                    let ReceivePhase::Listening { rx, .. } = std::mem::replace(&mut state.phase, ReceivePhase::Idle) else {
                        unreachable!()
                    };
                    let _ = rx.finish(48_000);
                }
                ctx.request_repaint();
            }
        }
    });

    if !state.pending.is_empty() {
        ctx.request_repaint();
    }
}
