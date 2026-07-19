//! A simplified copy of `app_gui`'s live waterfall, tuned for
//! `channel_probe`'s 300-3400 Hz diagnostic sweep instead of any specific
//! modem's lane layout (no lane-divider markers, since there's nothing
//! scheme-specific to mark here). Kept as its own small copy rather than
//! sharing `app_gui`'s private module, so this diagnostic tool has no
//! dependency on whatever the physical layer happens to be this week.
use egui::{Color32, ColorImage, TextureHandle, TextureOptions};
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::sync::Arc;

const FFT_SIZE: usize = 2048;
const NUM_BINS: usize = 480;
const MAX_ROWS: usize = 400;
const HOP: usize = 200;
const MIN_DB_RANGE: f32 = 24.0;
const FLOOR_RISE: f32 = 0.06;
const FLOOR_FALL: f32 = 0.20;
const CEIL_RISE: f32 = 0.35;
const CEIL_FALL: f32 = 0.02;

pub struct Waterfall {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    rows: VecDeque<Vec<f32>>,
    floor_db: f32,
    ceil_db: f32,
    band_low_hz: f32,
    band_high_hz: f32,
    texture: Option<TextureHandle>,
}

impl Waterfall {
    pub fn new(band_low_hz: f32, band_high_hz: f32) -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| {
                let x = i as f32 / (FFT_SIZE - 1) as f32;
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
            })
            .collect();
        Self { fft, window, rows: VecDeque::with_capacity(MAX_ROWS), floor_db: -80.0, ceil_db: -20.0, band_low_hz, band_high_hz, texture: None }
    }

    pub fn push_samples(&mut self, samples: &[f32], sample_rate: u32) {
        if samples.len() < FFT_SIZE / 4 || sample_rate == 0 {
            return;
        }
        let mut pos = FFT_SIZE.min(samples.len());
        loop {
            let start = pos.saturating_sub(FFT_SIZE);
            self.push_one_row(&samples[start..pos], sample_rate);
            if pos >= samples.len() {
                break;
            }
            pos = (pos + HOP).min(samples.len());
        }
    }

    fn push_one_row(&mut self, samples: &[f32], sample_rate: u32) {
        let mut buf = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
        let n = samples.len().min(FFT_SIZE);
        let start = samples.len() - n;
        let pad = FFT_SIZE - n;
        for i in 0..n {
            buf[pad + i] = Complex32::new(samples[start + i] * self.window[pad + i], 0.0);
        }
        self.fft.process(&mut buf);

        let bin_hz = sample_rate as f32 / FFT_SIZE as f32;
        let mut row_db = vec![-120.0f32; NUM_BINS];
        let mut row_min = f32::MAX;
        let mut row_max = f32::MIN;
        for (i, out) in row_db.iter_mut().enumerate() {
            let freq = self.band_low_hz + (self.band_high_hz - self.band_low_hz) * i as f32 / (NUM_BINS - 1) as f32;
            let bin = ((freq / bin_hz).round() as usize).min(FFT_SIZE / 2 - 1);
            let mag = buf[bin].norm();
            let db = 20.0 * (mag + 1e-8).log10();
            *out = db;
            row_min = row_min.min(db);
            row_max = row_max.max(db);
        }

        let floor_rate = if row_min > self.floor_db { FLOOR_RISE } else { FLOOR_FALL };
        self.floor_db += (row_min - self.floor_db) * floor_rate;
        let ceil_rate = if row_max > self.ceil_db { CEIL_RISE } else { CEIL_FALL };
        self.ceil_db += (row_max - self.ceil_db) * ceil_rate;
        if self.ceil_db - self.floor_db < MIN_DB_RANGE {
            let mid = (self.ceil_db + self.floor_db) / 2.0;
            self.floor_db = mid - MIN_DB_RANGE / 2.0;
            self.ceil_db = mid + MIN_DB_RANGE / 2.0;
        }

        let range = (self.ceil_db - self.floor_db).max(1.0);
        let row: Vec<f32> = row_db.iter().map(|&db| ((db - self.floor_db) / range).clamp(0.0, 1.0)).collect();

        self.rows.push_front(row);
        while self.rows.len() > MAX_ROWS {
            self.rows.pop_back();
        }
    }

    fn to_color_image(&self) -> ColorImage {
        let mut pixels = vec![Color32::from_rgb(6, 8, 11); NUM_BINS * MAX_ROWS];
        for (y, row) in self.rows.iter().enumerate() {
            for (x, &v) in row.iter().enumerate() {
                pixels[y * NUM_BINS + x] = magnitude_to_color(v);
            }
        }
        ColorImage { size: [NUM_BINS, MAX_ROWS], pixels }
    }

    pub fn content(&mut self, ui: &mut egui::Ui) {
        let image = self.to_color_image();
        match &mut self.texture {
            Some(tex) => tex.set(image, TextureOptions::NEAREST),
            None => self.texture = Some(ui.ctx().load_texture("channel_probe_waterfall", image, TextureOptions::NEAREST)),
        }
        let texture = self.texture.as_ref().unwrap();

        let size = egui::vec2(ui.available_width(), (ui.available_height() - 24.0).max(60.0));
        let response = ui.add(egui::Image::new((texture.id(), size)).fit_to_exact_size(size));
        let rect = response.rect;

        let painter = ui.painter();
        let band_span = self.band_high_hz - self.band_low_hz;
        let tick_step = nice_tick_step(band_span);
        let mut f = (self.band_low_hz / tick_step).ceil() * tick_step;
        while f < self.band_high_hz {
            let frac = (f - self.band_low_hz) / band_span;
            let x = rect.left() + rect.width() * frac;
            painter.line_segment([egui::pos2(x, rect.bottom() - 6.0), egui::pos2(x, rect.bottom())], egui::Stroke::new(1.0_f32, Color32::from_white_alpha(70)));
            painter.text(
                egui::pos2(x, rect.bottom() + 2.0),
                egui::Align2::CENTER_TOP,
                format!("{f:.0}"),
                egui::FontId::monospace(9.0),
                Color32::from_white_alpha(120),
            );
            f += tick_step;
        }
    }
}

fn magnitude_to_color(v: f32) -> Color32 {
    const STOPS: [(f32, [u8; 3]); 6] = [
        (0.0, [6, 8, 11]),
        (0.22, [13, 38, 56]),
        (0.45, [20, 96, 112]),
        (0.66, [56, 168, 168]),
        (0.85, [230, 170, 80]),
        (1.0, [252, 244, 224]),
    ];
    let v = v.clamp(0.0, 1.0);
    for pair in STOPS.windows(2) {
        let (t0, c0) = pair[0];
        let (t1, c1) = pair[1];
        if v <= t1 {
            let t = if t1 > t0 { (v - t0) / (t1 - t0) } else { 0.0 };
            let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
            return Color32::from_rgb(lerp(c0[0], c1[0]), lerp(c0[1], c1[1]), lerp(c0[2], c1[2]));
        }
    }
    Color32::from_rgb(252, 244, 224)
}

fn nice_tick_step(span: f32) -> f32 {
    let raw = span / 6.0;
    let magnitude = 10f32.powf(raw.log10().floor());
    let residual = raw / magnitude;
    let step = if residual < 1.5 {
        1.0
    } else if residual < 3.5 {
        2.5
    } else if residual < 7.5 {
        5.0
    } else {
        10.0
    };
    (step * magnitude).max(10.0)
}
