//! Minimal, scheme-agnostic channel probe: before designing (or re-designing)
//! any physical layer, measure what the *real* link actually does to a
//! known signal -- frequency response across the voice band, noise floor,
//! and playback/capture timing drift -- rather than guessing from theory
//! or a simulated channel. Two real-hardware failures in a row (a chirp
//! scheme and then a tone-chord scheme) both looked fine in simulation and
//! were never checked against a real capture *before* committing to a full
//! implementation; this tool exists so that check can happen first, cheaply
//! and repeatably, independent of whatever modulation scheme is tried next.
//!
//! Deliberately has zero dependency on `hsm_modem`/`tone_modem`/`cfs_fec` --
//! it should still be useful no matter how many more times the physical
//! layer changes.
//!
//! Usage (radios are normally on two separate machines, so `play` and
//! `listen` are run separately -- see each command's own note below):
//!   cargo run --release --example channel_probe -p audio_io -- list
//!   cargo run --release --example channel_probe -p audio_io -- generate probe.wav
//!   cargo run --release --example channel_probe -p audio_io -- play "<output device>"
//!   cargo run --release --example channel_probe -p audio_io -- listen "<input device>" 20 captured.wav
//!   cargo run --release --example channel_probe -p audio_io -- analyze captured.wav
use std::f32::consts::PI;
use std::io::{Read, Write};
use std::path::Path;

const SAMPLE_RATE: u32 = 48_000;
const TARGET_PEAK: f32 = 0.9;

const MARKER_LOW_HZ: f32 = 300.0;
const MARKER_HIGH_HZ: f32 = 3_400.0;
const MARKER_DURATION_S: f32 = 0.25;

const SILENCE_DURATION_S: f32 = 0.7;

const TONE_START_HZ: f32 = 300.0;
const TONE_END_HZ: f32 = 3_300.0;
const TONE_STEP_HZ: f32 = 200.0;
const TONE_DURATION_S: f32 = 0.3;
const TONE_GAP_S: f32 = 0.1;

#[derive(Clone, Copy, Debug)]
enum SegmentKind {
    Marker,
    Silence,
    Tone(f32),
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    kind: SegmentKind,
    start_sample: usize,
    len_samples: usize,
}

/// Builds the full diagnostic waveform (marker, silence, a stepped tone
/// sweep across the voice band, then a closing marker) plus the segment
/// map describing where each part starts and how long it is. Fully
/// deterministic -- the analyzer rebuilds this exact same signal in memory
/// to use as its reference, so no separate "TX reference" file is ever
/// needed, only the captured RX audio.
fn build_signal() -> (Vec<f32>, Vec<Segment>) {
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

fn generate_chirp(f_lo: f32, f_hi: f32, n_samples: usize, sample_rate: u32) -> Vec<f32> {
    let duration = n_samples as f32 / sample_rate as f32;
    let bw = f_hi - f_lo;
    (0..n_samples)
        .map(|n| {
            let t = n as f32 / sample_rate as f32;
            (2.0 * PI * (f_lo * t + (bw / (2.0 * duration)) * t * t)).sin()
        })
        .collect()
}

fn generate_tone(freq: f32, n_samples: usize, sample_rate: u32) -> Vec<f32> {
    (0..n_samples).map(|n| (2.0 * PI * freq * n as f32 / sample_rate as f32).sin()).collect()
}

fn normalize_peak(buf: &mut [f32], target_peak: f32) {
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

fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

fn correlate_matched(segment: &[f32], template: &[f32], template_norm: f32) -> f32 {
    if template_norm < 1e-9 {
        0.0
    } else {
        dot(segment, template) / template_norm
    }
}

fn goertzel_mag(samples: &[f32], sample_rate: u32, freq: f32) -> f32 {
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

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        0.0
    } else {
        (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
    }
}

/// Coarse-to-fine matched-filter search for `template` within `audio`,
/// over `search_range` -- the same strategy `hsm_modem::find_preamble_sync`
/// used, reimplemented standalone here so this tool has no dependency on
/// any modem crate. Returns (best offset, best correlation score).
fn find_marker(audio: &[f32], template: &[f32], template_norm: f32, search_range: std::ops::Range<usize>) -> (usize, f32) {
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

fn write_wav_f32(path: &Path, sample_rate: u32, samples: &[f32]) -> std::io::Result<()> {
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

fn read_wav_f32(path: &Path) -> Result<(u32, Vec<f32>), String> {
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

fn analyze(captured_path: &str) {
    let (rate, raw) = match read_wav_f32(Path::new(captured_path)) {
        Ok(v) => v,
        Err(e) => {
            println!("failed to read {captured_path}: {e}");
            return;
        }
    };
    let captured = if rate != SAMPLE_RATE { audio_io::resample(&raw, rate, SAMPLE_RATE) } else { raw };
    println!("loaded {captured_path}: {:.2}s captured at {rate} Hz (resampled to {SAMPLE_RATE} Hz for analysis)\n", captured.len() as f32 / rate as f32);

    let peak = captured.iter().fold(0.0f32, |acc, &x| acc.max(x.abs()));
    println!("overall peak amplitude: {peak:.5}  (1.0 = full scale -- if this is near 0, no meaningful signal reached the RX device at all)");
    println!("RMS envelope, 250ms windows (skim for where the loudest activity actually is):");
    let window = (SAMPLE_RATE as f32 * 0.25) as usize;
    let mut i = 0;
    let mut col = 0;
    while i < captured.len() {
        let end = (i + window).min(captured.len());
        print!("{:>7.4} ", rms(&captured[i..end]));
        col += 1;
        if col % 8 == 0 {
            println!();
        }
        i = end;
    }
    println!("\n");

    let (reference, segments) = build_signal();
    let marker_segment = segments[0];
    let marker_ref = &reference[marker_segment.start_sample..marker_segment.start_sample + marker_segment.len_samples];
    let marker_norm = norm(marker_ref);

    let (start_offset, start_score) = find_marker(&captured, marker_ref, marker_norm, 0..captured.len());
    println!("start marker found at sample {start_offset} (correlation score {start_score:.4})");
    if start_score < marker_norm * 0.05 {
        println!("WARNING: correlation score is very low relative to the marker's own energy -- the start marker may not have been found at all. Results below are unreliable.\n");
    }

    let last = segments.last().unwrap();
    let expected_end_marker_offset = start_offset + last.start_sample;
    let end_search_lo = expected_end_marker_offset.saturating_sub(SAMPLE_RATE as usize / 2);
    let end_search_hi = (expected_end_marker_offset + SAMPLE_RATE as usize / 2).min(captured.len());
    let (end_offset, end_score) = find_marker(&captured, marker_ref, marker_norm, end_search_lo..end_search_hi);
    let drift_samples = end_offset as i64 - expected_end_marker_offset as i64;
    let nominal_span = (last.start_sample - marker_segment.start_sample) as f64;
    let drift_ppm = if nominal_span > 0.0 { (drift_samples as f64 / nominal_span) * 1_000_000.0 } else { 0.0 };
    println!(
        "end marker found at sample {end_offset} (expected {expected_end_marker_offset}, correlation score {end_score:.4}) -- drift: {drift_samples} samples ({drift_ppm:.1} ppm)\n"
    );

    let mut noise_floor = 0.0f32;
    println!("{:>8}  {:>12}  {:>12}  {:>9}  {:>8}", "Hz", "rx_mag", "ref_mag", "ratio_dB", "SNR_dB");
    for seg in &segments {
        let seg_start = start_offset + seg.start_sample;
        let seg_end = seg_start + seg.len_samples;
        if seg_end > captured.len() {
            continue;
        }
        let rx_slice = &captured[seg_start..seg_end];
        match seg.kind {
            SegmentKind::Silence => {
                noise_floor = rms(rx_slice);
                println!("{:>8}  measured noise floor (RMS): {:.6}", "silence", noise_floor);
            }
            SegmentKind::Tone(freq) => {
                let ref_slice = &reference[seg.start_sample..seg.start_sample + seg.len_samples];
                let ref_mag = goertzel_mag(ref_slice, SAMPLE_RATE, freq);
                let rx_mag = goertzel_mag(rx_slice, SAMPLE_RATE, freq);
                let ratio_db = 20.0 * (rx_mag.max(1e-9) / ref_mag.max(1e-9)).log10();
                let noise_mag = goertzel_mag(rx_slice, SAMPLE_RATE, freq).max(1e-9);
                let snr_db = if noise_floor > 1e-9 { 20.0 * (noise_mag / (noise_floor * (rx_slice.len() as f32).sqrt())).log10() } else { f32::INFINITY };
                let bar_len = ((ratio_db + 60.0).max(0.0) / 2.0) as usize;
                let bar: String = "#".repeat(bar_len.min(40));
                println!("{freq:>8.0}  {rx_mag:>12.4}  {ref_mag:>12.4}  {ratio_db:>9.1}  {snr_db:>8.1}  {bar}");
            }
            SegmentKind::Marker => {}
        }
    }
    println!("\nratio_dB is this tone's measured level relative to what a perfect, undamaged copy of the reference signal would score -- 0 dB means it survived essentially perfectly, very negative means the channel attenuated it heavily at that frequency.");
}

fn cmd_generate(args: &[String]) {
    let out_path = args.first().map(|s| s.as_str()).unwrap_or("probe.wav");
    let (wave, segments) = build_signal();
    if let Err(e) = write_wav_f32(Path::new(out_path), SAMPLE_RATE, &wave) {
        println!("failed to write {out_path}: {e}");
        return;
    }
    println!("wrote {out_path}: {:.2}s, {} segments", wave.len() as f32 / SAMPLE_RATE as f32, segments.len());
}

fn cmd_play(args: &[String]) {
    let Some(device) = args.first() else {
        println!("usage: channel_probe play <output device name>");
        return;
    };
    let (wave, _segments) = build_signal();
    println!("playing {:.2}s diagnostic signal out \"{device}\" -- key up PTT now if it isn't already", wave.len() as f32 / SAMPLE_RATE as f32);
    match audio_io::start_transmission(device, wave, SAMPLE_RATE) {
        Ok(handle) => {
            while !handle.is_finished() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            println!("done.");
        }
        Err(e) => println!("failed to start playback: {e}"),
    }
}

fn cmd_listen(args: &[String]) {
    if args.len() < 3 {
        println!("usage: channel_probe listen <input device name> <seconds> <output.wav>");
        println!("(start listening a few seconds before the other side starts \"play\" -- the analyzer searches the whole capture for the start marker, so extra lead-in/lead-out is harmless)");
        return;
    }
    let device = &args[0];
    let seconds: f32 = match args[1].parse() {
        Ok(v) => v,
        Err(_) => {
            println!("couldn't parse seconds: {}", args[1]);
            return;
        }
    };
    let out_path = &args[2];

    println!("listening on \"{device}\" for {seconds:.1}s...");
    match audio_io::start_reception(device) {
        Ok(handle) => {
            std::thread::sleep(std::time::Duration::from_secs_f32(seconds));
            let samples = handle.finish(SAMPLE_RATE);
            if let Err(e) = write_wav_f32(Path::new(out_path), SAMPLE_RATE, &samples) {
                println!("failed to write {out_path}: {e}");
                return;
            }
            println!("wrote {out_path}: {:.2}s captured", samples.len() as f32 / SAMPLE_RATE as f32);
        }
        Err(e) => println!("failed to start capture: {e}"),
    }
}

/// Prints a numbered list of `devices` (the same names the GUI's device
/// dropdown would show) and prompts for a choice -- by index, or the exact
/// device name typed directly. Enter alone picks the first device.
fn prompt_device_choice(devices: &[audio_io::DeviceInfo], label: &str) -> Option<String> {
    if devices.is_empty() {
        println!("No {label} devices found.");
        return None;
    }
    println!("{label} devices:");
    for (i, d) in devices.iter().enumerate() {
        println!("  [{i}] {}", d.name);
    }
    print!("Select {label} device [0]: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return devices.first().map(|d| d.name.clone());
    }
    // Some shells (PowerShell's `|` to a native process, in particular)
    // inject a leading UTF-8 BOM into piped stdin -- strip it so a piped
    // "3\n" parses as the index 3, not the literal string "\u{FEFF}3".
    let trimmed = line.trim_start_matches('\u{FEFF}').trim();
    if trimmed.is_empty() {
        return devices.first().map(|d| d.name.clone());
    }
    if let Ok(idx) = trimmed.parse::<usize>() {
        if let Some(d) = devices.get(idx) {
            return Some(d.name.clone());
        }
        println!("index {idx} out of range, using [0] instead");
        return devices.first().map(|d| d.name.clone());
    }
    Some(trimmed.to_string())
}

/// One-shot same-machine test: pick RX and TX devices interactively (like
/// the GUI's device dropdowns), start capturing, play the diagnostic
/// signal, save the capture, and analyze it immediately -- no manual
/// two-machine timing coordination needed since both ends are local.
fn cmd_run() {
    let inputs = match audio_io::list_input_devices() {
        Ok(v) => v,
        Err(e) => {
            println!("failed to list input devices: {e}");
            return;
        }
    };
    let outputs = match audio_io::list_output_devices() {
        Ok(v) => v,
        Err(e) => {
            println!("failed to list output devices: {e}");
            return;
        }
    };

    let Some(input) = prompt_device_choice(&inputs, "input (RX)") else {
        return;
    };
    let Some(output) = prompt_device_choice(&outputs, "output (TX)") else {
        return;
    };

    let (wave, _segments) = build_signal();
    let signal_seconds = wave.len() as f32 / SAMPLE_RATE as f32;
    const LEAD_IN_S: f32 = 1.5;
    const LEAD_OUT_S: f32 = 2.0;

    println!("\nRX: \"{input}\"   TX: \"{output}\"");
    println!("Starting capture...");
    let rx_handle = match audio_io::start_reception(&input) {
        Ok(h) => h,
        Err(e) => {
            println!("failed to start capture: {e}");
            return;
        }
    };

    std::thread::sleep(std::time::Duration::from_secs_f32(LEAD_IN_S));

    println!("Key up PTT now -- playback starts in:");
    for n in (1..=3).rev() {
        println!("  {n}...");
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    println!("Playing {signal_seconds:.2}s diagnostic signal...");
    match audio_io::start_transmission(&output, wave, SAMPLE_RATE) {
        Ok(tx_handle) => {
            while !tx_handle.is_finished() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        Err(e) => {
            println!("failed to start playback: {e}");
            let _ = rx_handle.finish(SAMPLE_RATE);
            return;
        }
    }

    println!("Playback finished, capturing {LEAD_OUT_S:.1}s more tail...");
    std::thread::sleep(std::time::Duration::from_secs_f32(LEAD_OUT_S));

    let captured = rx_handle.finish(SAMPLE_RATE);
    let out_path = "channel_probe_capture.wav";
    if let Err(e) = write_wav_f32(Path::new(out_path), SAMPLE_RATE, &captured) {
        println!("failed to write {out_path}: {e}");
        return;
    }
    println!("saved capture to {out_path} ({:.2}s)\n", captured.len() as f32 / SAMPLE_RATE as f32);

    analyze(out_path);
}

fn cmd_list() {
    match audio_io::list_input_devices() {
        Ok(devices) => {
            println!("Input devices ({}):", devices.len());
            for d in devices {
                println!("  - {}", d.name);
            }
        }
        Err(e) => println!("Failed to enumerate input devices: {e}"),
    }
    match audio_io::list_output_devices() {
        Ok(devices) => {
            println!("Output devices ({}):", devices.len());
            for d in devices {
                println!("  - {}", d.name);
            }
        }
        Err(e) => println!("Failed to enumerate output devices: {e}"),
    }
}

fn print_usage() {
    println!("channel_probe -- measure what a real audio link does to a known signal, before designing a physical layer around it.");
    println!();
    println!("commands:");
    println!("  run                                            interactive: pick RX/TX devices from a list, play+capture+analyze in one shot (same machine, two devices)");
    println!("  list                                          list input/output devices");
    println!("  generate <out.wav>                            write the diagnostic signal to a WAV file (for playback via other means)");
    println!("  play <output device>                          play the diagnostic signal live (same code path the real app uses to transmit)");
    println!("  listen <input device> <seconds> <out.wav>     capture live for <seconds> and save to <out.wav>");
    println!("  analyze <captured.wav>                        report frequency response, noise floor, and timing drift");
    println!();
    println!("same machine, two devices: use \"run\" -- it lists devices and prompts, like the GUI's dropdowns.");
    println!("two separate radios/machines: start \"listen\" on the RX machine first, then run \"play\" on the TX machine, then \"analyze\" the captured file.");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("run") => cmd_run(),
        Some("list") => cmd_list(),
        Some("generate") => cmd_generate(&args[2..]),
        Some("play") => cmd_play(&args[2..]),
        Some("listen") => cmd_listen(&args[2..]),
        Some("analyze") => {
            if let Some(path) = args.get(2) {
                analyze(path);
            } else {
                println!("usage: channel_probe analyze <captured.wav>");
            }
        }
        _ => print_usage(),
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

        // Simulate a capture starting partway through some silence, i.e. an
        // unknown lead-in the analyzer has to discover.
        let lead_in = vec![0.0f32; 5_000];
        let mut captured = lead_in.clone();
        captured.extend(&wave);

        let (start_offset, score) = find_marker(&captured, marker_ref, marker_norm, 0..captured.len());
        assert_eq!(start_offset, lead_in.len());
        assert!(score > marker_norm * 0.9, "matched-filter score should be strong at the true offset: {score} vs norm {marker_norm}");

        // A tone segment should score much higher at its own frequency than at an unrelated one.
        let tone_seg = segments.iter().find(|s| matches!(s.kind, SegmentKind::Tone(f) if (f - 1500.0).abs() < 1.0)).expect("1500Hz tone should exist");
        let seg_start = start_offset + tone_seg.start_sample;
        let rx_slice = &captured[seg_start..seg_start + tone_seg.len_samples];
        let mag_at_freq = goertzel_mag(rx_slice, SAMPLE_RATE, 1500.0);
        let mag_off_freq = goertzel_mag(rx_slice, SAMPLE_RATE, 700.0);
        assert!(mag_at_freq > mag_off_freq * 5.0, "the 1500Hz segment should score far higher at 1500Hz than at 700Hz: {mag_at_freq} vs {mag_off_freq}");
    }

    #[test]
    fn silence_segment_has_near_zero_energy_in_the_clean_reference() {
        let (reference, segments) = build_signal();
        let silence = segments.iter().find(|s| matches!(s.kind, SegmentKind::Silence)).expect("silence segment should exist");
        let slice = &reference[silence.start_sample..silence.start_sample + silence.len_samples];
        assert!(rms(slice) < 1e-6);
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
        assert!(tones.len() >= 10, "should have decent frequency resolution: only {} tones", tones.len());
    }
}
