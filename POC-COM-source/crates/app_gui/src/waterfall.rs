//! An SDR-style waterfall: frequency across, time scrolling down, color for
//! magnitude -- the same instrument as HDSDR's, tuned for HSM's band.
//! Embedded as a toggleable right-side panel (not a floating popup) so it
//! has real room to show detail, especially with the main window
//! maximized. Both tabs share this one implementation -- only what feeds
//! it (a cached transmit waveform vs. a live capture tail) differs.

use egui::{Color32, ColorImage, TextureHandle, TextureOptions};
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 2048 @ 48 kHz = ~23 Hz/bin. This is deliberately *not* pushed higher:
/// HSM's symbols are 35 ms, so a longer FFT window starts averaging two
/// different symbols' (different) shift values together, blurring exactly
/// the per-symbol sweep this display exists to show. Bin count and row
/// count (below) are where the extra resolution the display needs
/// actually belongs.
const FFT_SIZE: usize = 2048;
const NUM_BINS: usize = 480;
const MAX_ROWS: usize = 560;
/// Samples advanced per row. At 48 kHz this is ~4.2 ms/row -- dense enough
/// that a 35 ms symbol spans ~8 rows, giving a visibly continuous diagonal
/// rather than 1-2 blocky steps.
const HOP: usize = 200;
const MIN_DB_RANGE: f32 = 24.0;
const FLOOR_RISE: f32 = 0.06;
const FLOOR_FALL: f32 = 0.20;
const CEIL_RISE: f32 = 0.35;
const CEIL_FALL: f32 = 0.02;

struct WaterfallBuffer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    rows: VecDeque<Vec<f32>>,
    floor_db: f32,
    ceil_db: f32,
    band_low_hz: f32,
    band_high_hz: f32,
}

impl WaterfallBuffer {
    fn new(band_low_hz: f32, band_high_hz: f32) -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        // Hann window: cuts spectral leakage so lane edges read as clean
        // vertical bands rather than smeared columns.
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| {
                let x = i as f32 / (FFT_SIZE - 1) as f32;
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
            })
            .collect();
        Self { fft, window, rows: VecDeque::with_capacity(MAX_ROWS), floor_db: -80.0, ceil_db: -20.0, band_low_hz, band_high_hz }
    }

    /// Processes `samples` (a recent window, e.g. the last ~1/8s) in dense
    /// `HOP`-sized steps, pushing one row per step -- this is what makes
    /// the live feed match the density of the offline validation test
    /// instead of one row per (much coarser) UI poll.
    fn push_samples(&mut self, samples: &[f32], sample_rate: u32) {
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

        // Asymmetric EMA: the floor rises slowly (so a moment of quiet
        // doesn't instantly crush the contrast) but falls fast (so it
        // still finds true silence quickly); the ceiling does the mirror
        // image, so a strong chirp snaps the display into contrast almost
        // immediately, the way an SDR's auto-level does.
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
}

/// Dark navy -> teal -> amber -> warm white, echoing the project's own
/// accent/warning palette rather than a default SDR jet colormap.
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

/// The toggleable waterfall. One instance per tab (Send/Receive each own
/// their own, since they watch different signals); `app.rs` decides
/// whether to embed the currently-active tab's instance in a side panel.
pub struct WaterfallWindow {
    pub open: bool,
    buffer: WaterfallBuffer,
    texture: Option<TextureHandle>,
    last_feed: Instant,
}

impl WaterfallWindow {
    /// How often the caller should call `feed`. Each call processes dense
    /// internal `HOP` steps regardless, so this only needs to be "close to
    /// every frame," not fine-grained itself.
    const POLL_INTERVAL: Duration = Duration::from_millis(15);
    /// How much recent audio to hand `feed` each call -- generous overlap
    /// with the previous call is intentional (cheap, and guarantees no
    /// gaps from frame-timing jitter) rather than tracking exact absolute
    /// sample positions across two very different data sources (a cached
    /// waveform vs. a live capture tail).
    pub const FEED_WINDOW_SAMPLES: usize = FFT_SIZE + HOP * 6;

    pub fn new(band_low_hz: f32, band_high_hz: f32) -> Self {
        Self { open: false, buffer: WaterfallBuffer::new(band_low_hz, band_high_hz), texture: None, last_feed: Instant::now() }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn wants_feed(&self) -> bool {
        self.open && self.last_feed.elapsed() >= Self::POLL_INTERVAL
    }

    pub fn feed(&mut self, samples: &[f32], sample_rate: u32) {
        self.last_feed = Instant::now();
        self.buffer.push_samples(samples, sample_rate);
    }

    /// Draws the waterfall's content into whatever container the caller
    /// provides (a `SidePanel` in practice) -- no window chrome, since
    /// this is embedded rather than popped out.
    pub fn content(&mut self, ui: &mut egui::Ui, data_lane_count: usize) {
        let image = self.buffer.to_color_image();
        match &mut self.texture {
            Some(tex) => tex.set(image, TextureOptions::NEAREST),
            None => self.texture = Some(ui.ctx().load_texture("waterfall", image, TextureOptions::NEAREST)),
        }
        let texture = self.texture.as_ref().unwrap();

        ui.label(egui::RichText::new("Live spectrum").weak().small());
        ui.add_space(4.0);

        let size = egui::vec2(ui.available_width(), (ui.available_height() - 34.0).max(60.0));
        let response = ui.add(egui::Image::new((texture.id(), size)).fit_to_exact_size(size));
        let rect = response.rect;

        let painter = ui.painter();
        let lanes = data_lane_count.max(1);
        for i in 1..lanes {
            let x = rect.left() + rect.width() * (i as f32 / lanes as f32);
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                egui::Stroke::new(1.0_f32, Color32::from_white_alpha(30)),
            );
        }

        // Frequency ruler: tick marks every ~300 Hz rather than just the
        // two band-edge labels, closer to an SDR's frequency axis.
        let band_span = self.buffer.band_high_hz - self.buffer.band_low_hz;
        let tick_step = nice_tick_step(band_span);
        let mut f = (self.buffer.band_low_hz / tick_step).ceil() * tick_step;
        while f < self.buffer.band_high_hz {
            let frac = (f - self.buffer.band_low_hz) / band_span;
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
        ui.add_space(14.0);
    }
}

/// Pick a round-ish tick spacing (in Hz) that gives roughly 5-8 ticks
/// across the given span.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Offline proof that the waterfall actually resolves this scheme's
    /// real clip-assembled speech: assemble a real short message, feed
    /// it through the exact same `push_samples` path (same hop density)
    /// the live GUI uses, and write it to a PNG for visual inspection.
    /// Uses the shared, fixed `lexicon_modem::display_band()` (no discrete
    /// tones/lanes to frame narrowly anymore -- just the real
    /// speech-relevant range).
    #[test]
    fn renders_real_lexicon_waveform_to_png() {
        let sample_rate = lexicon_modem::SAMPLE_RATE;
        let wave = lexicon_modem::assemble_from_tokens(&["READY", "COMPLETE"], 150);

        let (band_low, band_high) = lexicon_modem::display_band();
        let mut buffer = WaterfallBuffer::new(band_low, band_high);
        // Simulate the live GUI exactly: repeated `feed`-sized windows
        // handed to `push_samples`, not one giant call over the whole wave.
        let feed_window = WaterfallWindow::FEED_WINDOW_SAMPLES;
        let step = HOP * 4; // roughly what elapses between POLL_INTERVAL-spaced UI calls
        let mut pos = feed_window.min(wave.len());
        loop {
            let start = pos.saturating_sub(feed_window);
            buffer.push_samples(&wave[start..pos], sample_rate);
            if pos >= wave.len() {
                break;
            }
            pos = (pos + step).min(wave.len());
        }

        let image = buffer.to_color_image();
        let mut img = image::RgbImage::new(image.size[0] as u32, image.size[1] as u32);
        for (i, px) in image.pixels.iter().enumerate() {
            let x = (i % image.size[0]) as u32;
            let y = (i / image.size[0]) as u32;
            img.put_pixel(x, y, image::Rgb([px.r(), px.g(), px.b()]));
        }
        let out_path = std::env::temp_dir().join("poc_com_waterfall_offline_test.png");
        img.save(&out_path).expect("save waterfall PNG");
        eprintln!("wrote {}", out_path.display());
        let upscaled = image::imageops::resize(&img, img.width() * 2, img.height() * 2, image::imageops::FilterType::Nearest);
        let upscaled_path = std::env::temp_dir().join("poc_com_waterfall_offline_test_2x.png");
        upscaled.save(&upscaled_path).expect("save upscaled waterfall PNG");
        eprintln!("wrote {}", upscaled_path.display());

        let distinct_colors: std::collections::HashSet<_> = image.pixels.iter().map(|c| (c.r(), c.g(), c.b())).collect();
        assert!(distinct_colors.len() > 10, "waterfall image looks flat -- only {} distinct colors", distinct_colors.len());
    }
}
