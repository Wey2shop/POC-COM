//! Scheme-agnostic real-channel diagnostic: a known signal (wideband marker
//! + silence + a stepped tone sweep across the voice band) and the analysis
//! that measures what a real link actually did to it -- frequency
//! response, noise floor, and TX/RX timing drift -- before designing (or
//! re-designing) a physical layer around assumptions instead of a
//! measurement. Used by both a CLI (`crates/audio_io/examples/channel_probe.rs`)
//! and a GUI (`crates/channel_probe_gui`) so the two don't drift apart.
//!
//! Deliberately has no dependency on `hsm_modem`/`tone_modem`/`cfs_fec` --
//! it should stay useful no matter how many more times the physical layer
//! changes.
use std::f32::consts::PI;

pub const SAMPLE_RATE: u32 = 48_000;
pub const TARGET_PEAK: f32 = 0.9;

pub const MARKER_LOW_HZ: f32 = 300.0;
pub const MARKER_HIGH_HZ: f32 = 3_400.0;
pub const MARKER_DURATION_S: f32 = 0.25;

pub const SILENCE_DURATION_S: f32 = 0.7;

pub const TONE_START_HZ: f32 = 300.0;
pub const TONE_END_HZ: f32 = 3_300.0;
pub const TONE_STEP_HZ: f32 = 200.0;
pub const TONE_DURATION_S: f32 = 0.3;
pub const TONE_GAP_S: f32 = 0.1;

#[derive(Clone, Copy, Debug)]
pub enum SegmentKind {
    Marker,
    Silence,
    Tone(f32),
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub kind: SegmentKind,
    pub start_sample: usize,
    pub len_samples: usize,
}

/// Builds the full diagnostic waveform (marker, silence, a stepped tone
/// sweep across the voice band, then a closing marker) plus the segment
/// map describing where each part starts and how long it is. Fully
/// deterministic -- the analyzer rebuilds this exact same signal in memory
/// to use as its reference, so no separate "TX reference" file is ever
/// needed, only the captured RX audio.
pub fn build_signal() -> (Vec<f32>, Vec<Segment>) {
    let mut wave = Vec::new();
    let mut segments = Vec::new();

    let push = |kind: SegmentKind, samples: Vec<f32>, wave: &mut Vec<f32>, segments: &mut Vec<Segment>| {
        let start_sample = wave.len();
        let len_samples = samples.len();
        wave.extend(samples);
        segments.push(Segment { kind, start_sample, len_samples });
    };

    let marker_n = (SAMPLE_RATE as f32 * MARKER_DURATION_S).round() as usize;
    push(SegmentKind::Marker, generate_chirp(MARKER_LOW_HZ, MARKER_HIGH_HZ, marker_n, SAMPLE_RATE), &mut wave, &mut segments);

    let silence_n = (SAMPLE_RATE as f32 * SILENCE_DURATION_S).round() as usize;
    push(SegmentKind::Silence, vec![0.0f32; silence_n], &mut wave, &mut segments);

    let tone_n = (SAMPLE_RATE as f32 * TONE_DURATION_S).round() as usize;
    let gap_n = (SAMPLE_RATE as f32 * TONE_GAP_S).round() as usize;
    let mut freq = TONE_START_HZ;
    while freq <= TONE_END_HZ + 1e-3 {
        push(SegmentKind::Tone(freq), generate_tone(freq, tone_n, SAMPLE_RATE), &mut wave, &mut segments);
        wave.extend(vec![0.0f32; gap_n]);
        freq += TONE_STEP_HZ;
    }

    push(SegmentKind::Marker, generate_chirp(MARKER_LOW_HZ, MARKER_HIGH_HZ, marker_n, SAMPLE_RATE), &mut wave, &mut segments);

    normalize_peak(&mut wave, TARGET_PEAK);
    (wave, segments)
}

pub fn generate_chirp(f_lo: f32, f_hi: f32, n_samples: usize, sample_rate: u32) -> Vec<f32> {
    let duration = n_samples as f32 / sample_rate as f32;
    let bw = f_hi - f_lo;
    (0..n_samples)
        .map(|n| {
            let t = n as f32 / sample_rate as f32;
            (2.0 * PI * (f_lo * t + (bw / (2.0 * duration)) * t * t)).sin()
        })
        .collect()
}

pub fn generate_tone(freq: f32, n_samples: usize, sample_rate: u32) -> Vec<f32> {
    (0..n_samples).map(|n| (2.0 * PI * freq * n as f32 / sample_rate as f32).sin()).collect()
}

pub fn normalize_peak(buf: &mut [f32], target_peak: f32) {
    let peak = buf.iter().fold(0.0f32, |acc, &x| acc.max(x.abs()));
    if peak > 1e-9 {
        let scale = target_peak / peak;
        for x in buf.iter_mut() {
            *x *= scale;
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

pub fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

pub fn correlate_matched(segment: &[f32], template: &[f32], template_norm: f32) -> f32 {
    if template_norm < 1e-9 {
        0.0
    } else {
        dot(segment, template) / template_norm
    }
}

pub fn goertzel_mag(samples: &[f32], sample_rate: u32, freq: f32) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = (0.5 + (n as f32 * freq) / sample_rate as f32) as usize;
    let w = 2.0 * PI * k as f32 / n as f32;
    let cw = w.cos();
    let coeff = 2.0 * cw;
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt()
}

pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        0.0
    } else {
        (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
    }
}

/// Coarse-to-fine matched-filter search for `template` within `audio`,
/// over `search_range`. Returns (best offset, best correlation score).
pub fn find_marker(audio: &[f32], template: &[f32], template_norm: f32, search_range: std::ops::Range<usize>) -> (usize, f32) {
    const COARSE_STRIDE: usize = 8;
    let n = template.len();
    if audio.len() < n {
        return (search_range.start, 0.0);
    }
    let start = search_range.start;
    let end = search_range.end.min(audio.len() - n + 1);
    if end <= start {
        return (start, 0.0);
    }

    let mut best_offset = start;
    let mut best_score = f32::MIN;
    let mut offset = start;
    while offset < end {
        let s = correlate_matched(&audio[offset..offset + n], template, template_norm);
        if s > best_score {
            best_score = s;
            best_offset = offset;
        }
        offset += COARSE_STRIDE;
    }
    let fine_start = best_offset.saturating_sub(COARSE_STRIDE);
    let fine_end = (best_offset + COARSE_STRIDE + 1).min(end);
    for offset in fine_start..fine_end {
        let s = correlate_matched(&audio[offset..offset + n], template, template_norm);
        if s > best_score {
            best_score = s;
            best_offset = offset;
        }
    }
    (best_offset, best_score)
}

#[derive(Clone, Copy, Debug)]
pub struct ToneMeasurement {
    pub freq: f32,
    pub rx_mag: f32,
    pub ref_mag: f32,
    pub ratio_db: f32,
}

#[derive(Clone, Debug)]
pub struct AnalysisReport {
    pub start_offset: usize,
    pub start_score: f32,
    pub marker_norm: f32,
    pub end_offset: usize,
    pub expected_end_offset: usize,
    pub drift_samples: i64,
    pub drift_ppm: f64,
    pub end_score: f32,
    pub noise_floor: f32,
    pub tones: Vec<ToneMeasurement>,
    pub overall_peak: f32,
}

impl AnalysisReport {
    /// A weak start-marker score relative to its own energy means the
    /// marker probably wasn't really found -- everything else in the
    /// report is then not meaningful (see `channel_probe_gui`/the CLI for
    /// how this is surfaced to the user).
    pub fn looks_reliable(&self) -> bool {
        self.start_score >= self.marker_norm * 0.05
    }
}

/// Runs the full analysis against `captured` (assumed already at
/// `SAMPLE_RATE`). Pure function, no I/O -- callers own reading the WAV
/// file (or live buffer) and displaying the result.
pub fn analyze_samples(captured: &[f32]) -> AnalysisReport {
    let (reference, segments) = build_signal();
    let marker_segment = segments[0];
    let marker_ref = &reference[marker_segment.start_sample..marker_segment.start_sample + marker_segment.len_samples];
    let marker_norm = norm(marker_ref);

    let (start_offset, start_score) = find_marker(captured, marker_ref, marker_norm, 0..captured.len());

    let last = segments.last().unwrap();
    let expected_end_marker_offset = start_offset + last.start_sample;
    let end_search_lo = expected_end_marker_offset.saturating_sub(SAMPLE_RATE as usize / 2);
    let end_search_hi = (expected_end_marker_offset + SAMPLE_RATE as usize / 2).min(captured.len());
    let (end_offset, end_score) = find_marker(captured, marker_ref, marker_norm, end_search_lo..end_search_hi);
    let drift_samples = end_offset as i64 - expected_end_marker_offset as i64;
    let nominal_span = (last.start_sample - marker_segment.start_sample) as f64;
    let drift_ppm = if nominal_span > 0.0 { (drift_samples as f64 / nominal_span) * 1_000_000.0 } else { 0.0 };

    let mut noise_floor = 0.0f32;
    let mut tones = Vec::new();
    for seg in &segments {
        let seg_start = start_offset + seg.start_sample;
        let seg_end = seg_start + seg.len_samples;
        if seg_end > captured.len() {
            continue;
        }
        let rx_slice = &captured[seg_start..seg_end];
        match seg.kind {
            SegmentKind::Silence => noise_floor = rms(rx_slice),
            SegmentKind::Tone(freq) => {
                let ref_slice = &reference[seg.start_sample..seg.start_sample + seg.len_samples];
                let ref_mag = goertzel_mag(ref_slice, SAMPLE_RATE, freq);
                let rx_mag = goertzel_mag(rx_slice, SAMPLE_RATE, freq);
                let ratio_db = 20.0 * (rx_mag.max(1e-9) / ref_mag.max(1e-9)).log10();
                tones.push(ToneMeasurement { freq, rx_mag, ref_mag, ratio_db });
            }
            SegmentKind::Marker => {}
        }
    }

    let overall_peak = captured.iter().fold(0.0f32, |acc, &x| acc.max(x.abs()));

    AnalysisReport {
        start_offset,
        start_score,
        marker_norm,
        end_offset,
        expected_end_offset: expected_end_marker_offset,
        drift_samples,
        drift_ppm,
        end_score,
        noise_floor,
        tones,
        overall_peak,
    }
}

pub mod wav {
    use std::io::{Read, Write};
    use std::path::Path;

    pub fn write_wav_f32(path: &Path, sample_rate: u32, samples: &[f32]) -> std::io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        let data_len = (samples.len() * 2) as u32;
        let byte_rate = sample_rate * 2;
        file.write_all(b"RIFF")?;
        file.write_all(&(36 + data_len).to_le_bytes())?;
        file.write_all(b"WAVE")?;
        file.write_all(b"fmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?;
        file.write_all(&sample_rate.to_le_bytes())?;
        file.write_all(&byte_rate.to_le_bytes())?;
        file.write_all(&2u16.to_le_bytes())?;
        file.write_all(&16u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&data_len.to_le_bytes())?;
        for &s in samples {
            let clamped = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            file.write_all(&clamped.to_le_bytes())?;
        }
        Ok(())
    }

    pub fn read_wav_f32(path: &Path) -> Result<(u32, Vec<f32>), String> {
        let mut file = std::fs::File::open(path).map_err(|e| format!("couldn't open WAV: {e}"))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).map_err(|e| format!("couldn't read WAV: {e}"))?;
        if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
            return Err("not a valid RIFF/WAVE file".into());
        }
        let mut pos = 12usize;
        let mut channels = 1u16;
        let mut sample_rate = 0u32;
        let mut data: Option<&[u8]> = None;
        while pos + 8 <= buf.len() {
            let id = &buf[pos..pos + 4];
            let len = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let body_start = pos + 8;
            let body_end = (body_start + len).min(buf.len());
            let body = &buf[body_start..body_end];
            match id {
                b"fmt " => {
                    if body.len() >= 16 {
                        channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
                        sample_rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                    }
                }
                b"data" => data = Some(body),
                _ => {}
            }
            pos = body_start + len + (len % 2);
        }
        let data = data.ok_or("no data chunk found in WAV")?;
        let ch = channels.max(1) as usize;
        let frames = data.len() / (2 * ch);
        let mut mono = Vec::with_capacity(frames);
        for i in 0..frames {
            let mut sum = 0i32;
            for c in 0..ch {
                let s = i * 2 * ch + c * 2;
                sum += i16::from_le_bytes(data[s..s + 2].try_into().unwrap()) as i32;
            }
            mono.push((sum as f32 / ch as f32) / 32768.0);
        }
        Ok((sample_rate, mono))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_round_trip_finds_all_segments_at_their_exact_offsets() {
        let (wave, segments) = build_signal();
        let (reference, _) = build_signal();
        assert_eq!(wave, reference, "build_signal should be fully deterministic");

        let marker_segment = segments[0];
        let marker_ref = &reference[marker_segment.start_sample..marker_segment.start_sample + marker_segment.len_samples];
        let marker_norm = norm(marker_ref);

        let lead_in = vec![0.0f32; 5_000];
        let mut captured = lead_in.clone();
        captured.extend(&wave);

        let (start_offset, score) = find_marker(&captured, marker_ref, marker_norm, 0..captured.len());
        assert_eq!(start_offset, lead_in.len());
        assert!(score > marker_norm * 0.9);
    }

    #[test]
    fn analyze_samples_reports_clean_signal_as_reliable_with_zero_drift() {
        let (wave, _) = build_signal();
        let report = analyze_samples(&wave);
        assert!(report.looks_reliable());
        assert_eq!(report.drift_samples, 0);
        assert!(report.noise_floor < 1e-6);
        for tone in &report.tones {
            assert!(tone.ratio_db.abs() < 0.1, "clean signal should show ~0dB ratio at {}: {}", tone.freq, tone.ratio_db);
        }
    }

    #[test]
    fn analyze_samples_flags_a_near_silent_capture_as_unreliable() {
        let noisy = vec![0.001f32; SAMPLE_RATE as usize * 10];
        let report = analyze_samples(&noisy);
        assert!(!report.looks_reliable());
    }

    #[test]
    fn tone_sweep_covers_the_expected_band() {
        let (_wave, segments) = build_signal();
        let tones: Vec<f32> = segments
            .iter()
            .filter_map(|s| match s.kind {
                SegmentKind::Tone(f) => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(*tones.first().unwrap(), TONE_START_HZ);
        assert!(*tones.last().unwrap() >= TONE_END_HZ - TONE_STEP_HZ);
        assert!(tones.len() >= 10);
    }

    #[test]
    fn wav_round_trip_preserves_samples() {
        let dir = std::env::temp_dir();
        let path = dir.join("channel_probe_lib_wav_roundtrip_test.wav");
        let samples: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.02).sin() * 0.5).collect();
        wav::write_wav_f32(&path, SAMPLE_RATE, &samples).expect("write should succeed");
        let (rate, read_back) = wav::read_wav_f32(&path).expect("read should succeed");
        assert_eq!(rate, SAMPLE_RATE);
        assert_eq!(read_back.len(), samples.len());
        let _ = std::fs::remove_file(&path);
    }
}
