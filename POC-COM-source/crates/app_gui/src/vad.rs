//! Simple energy-based activity detector for continuous-receive mode.
//! Auto-calibrates to ambient noise (no user-facing threshold/gain
//! control, consistent with the rest of this app), then watches for
//! energy rising well above that floor (a transmission starting) and
//! falling back to it for a sustained period (the transmission ending).
//!
//! `QUIET_HANG_SECONDS` just needs to be safely longer than the longest
//! natural pause *within* a spoken sentence (a breath, a pause before the
//! close word), not padded out to cover a fixed block length -- this
//! scheme has no block/frame structure at all anymore, just one
//! continuous TTS utterance per message (see `lexicon_modem`'s doc
//! comments for the earlier fixed-block design this replaced).

use std::time::Duration;

const CALIBRATION_SECONDS: f32 = 1.0;
const WINDOW_SECONDS: f32 = 0.1;
const QUIET_HANG_SECONDS: f32 = 2.0;
const MAX_ACTIVE_SECONDS: f32 = 600.0; // safety cap so a stuck-noisy channel can't grow the segment forever
const THRESHOLD_MULTIPLIER: f32 = 4.0;
const THRESHOLD_FLOOR: f32 = 0.003;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

enum State {
    Calibrating { energies: Vec<f32> },
    Waiting,
    Active { start_idx: usize, quiet_since_idx: Option<usize> },
}

pub enum VadEvent {
    None,
    SegmentReady { start_idx: usize, end_idx: usize },
}

pub struct VadTracker {
    device_rate: u32,
    noise_floor: f32,
    state: State,
}

impl VadTracker {
    pub fn new(device_rate: u32) -> Self {
        Self { device_rate, noise_floor: 0.0, state: State::Calibrating { energies: Vec::new() } }
    }

    pub fn poll_interval() -> Duration {
        POLL_INTERVAL
    }

    pub fn status_label(&self) -> &'static str {
        match self.state {
            State::Calibrating { .. } => "Calibrating to ambient noise...",
            State::Waiting => "Listening for a transmission...",
            State::Active { .. } => "Signal detected -- receiving...",
        }
    }

    /// Call roughly every `poll_interval()`. Reads only the recent tail of
    /// the capture buffer (cheap), not the whole thing.
    pub fn poll(&mut self, rx: &audio_io::RxHandle) -> VadEvent {
        let window_n = ((self.device_rate as f32 * WINDOW_SECONDS) as usize).max(1);
        let tail = rx.tail(window_n);
        if tail.is_empty() {
            return VadEvent::None;
        }
        let energy = rms(&tail);
        let total_len = rx.samples_captured();
        let threshold = (self.noise_floor * THRESHOLD_MULTIPLIER).max(THRESHOLD_FLOOR);

        match &mut self.state {
            State::Calibrating { energies } => {
                energies.push(energy);
                let need = ((CALIBRATION_SECONDS / WINDOW_SECONDS) as usize).max(1);
                if energies.len() >= need {
                    let mut sorted = energies.clone();
                    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    self.noise_floor = sorted[sorted.len() / 2];
                    self.state = State::Waiting;
                }
                VadEvent::None
            }
            State::Waiting => {
                if energy > threshold {
                    let start_idx = total_len.saturating_sub(window_n);
                    self.state = State::Active { start_idx, quiet_since_idx: None };
                }
                VadEvent::None
            }
            State::Active { start_idx, quiet_since_idx } => {
                if energy > threshold {
                    *quiet_since_idx = None;
                } else if quiet_since_idx.is_none() {
                    *quiet_since_idx = Some(total_len);
                }

                let quiet_long_enough = quiet_since_idx
                    .map(|q| (total_len.saturating_sub(q)) as f32 / self.device_rate as f32 >= QUIET_HANG_SECONDS)
                    .unwrap_or(false);
                let active_duration = total_len.saturating_sub(*start_idx) as f32 / self.device_rate as f32;

                if quiet_long_enough || active_duration >= MAX_ACTIVE_SECONDS {
                    let event = VadEvent::SegmentReady { start_idx: *start_idx, end_idx: quiet_since_idx.unwrap_or(total_len) };
                    self.state = State::Waiting;
                    event
                } else {
                    VadEvent::None
                }
            }
        }
    }
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}
