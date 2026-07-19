//! Minimal WAV read/write for the raw modulated waveform (not media audio
//! content -- that's `media_codec::audio_codec`). Lets a user save exactly
//! what would have been played out the speaker, and load exactly what
//! would have been captured from the mic, without touching audio hardware
//! at all -- useful both as a feature (offline test files, sharing a
//! transmission) and as a diagnostic that isolates the DSP pipeline from
//! the audio device/routing layer.

use std::io::{Read, Write};
use std::path::Path;

pub fn write_wav_f32(path: &Path, sample_rate: u32, samples: &[f32]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    let data_len = (samples.len() * 2) as u32; // PCM16 mono
    let byte_rate = sample_rate * 2;

    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_len).to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?; // PCM
    file.write_all(&1u16.to_le_bytes())?; // mono
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?; // block align
    file.write_all(&16u16.to_le_bytes())?; // bits per sample
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        let clamped = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        file.write_all(&clamped.to_le_bytes())?;
    }
    Ok(())
}

/// Reads a WAV file, downmixing to mono f32 in [-1, 1]. Supports PCM
/// 8/16/24/32-bit and IEEE float 32-bit, which covers the overwhelming
/// majority of real-world WAV files without pulling in a full parsing crate.
pub fn read_wav_f32(path: &Path) -> Result<(u32, Vec<f32>), String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("couldn't open WAV: {e}"))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(|e| format!("couldn't read WAV: {e}"))?;

    if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err("not a valid RIFF/WAVE file".into());
    }

    let mut pos = 12usize;
    let mut audio_format = 0u16;
    let mut channels = 1u16;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 16u16;
    let mut data: Option<&[u8]> = None;

    while pos + 8 <= buf.len() {
        let chunk_id = &buf[pos..pos + 4];
        let chunk_len = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + chunk_len).min(buf.len());
        let body = &buf[body_start..body_end];

        match chunk_id {
            b"fmt " => {
                if body.len() >= 16 {
                    audio_format = u16::from_le_bytes(body[0..2].try_into().unwrap());
                    channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
                    sample_rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                    bits_per_sample = u16::from_le_bytes(body[14..16].try_into().unwrap());
                }
            }
            b"data" => data = Some(body),
            _ => {}
        }

        pos = body_start + chunk_len + (chunk_len % 2); // chunks are word-aligned
    }

    let data = data.ok_or("no data chunk found in WAV")?;
    let channels = channels.max(1) as usize;
    let bytes_per_sample = (bits_per_sample / 8).max(1) as usize;

    let is_float = audio_format == 3;
    let frame_bytes = bytes_per_sample * channels;
    if frame_bytes == 0 {
        return Err("invalid WAV format (zero-size frame)".into());
    }
    let num_frames = data.len() / frame_bytes;

    let mut mono = Vec::with_capacity(num_frames);
    for frame_idx in 0..num_frames {
        let frame_start = frame_idx * frame_bytes;
        let mut sum = 0.0f32;
        for ch in 0..channels {
            let s = frame_start + ch * bytes_per_sample;
            let sample = decode_sample(&data[s..s + bytes_per_sample], bits_per_sample, is_float);
            sum += sample;
        }
        mono.push(sum / channels as f32);
    }

    Ok((sample_rate, mono))
}

fn decode_sample(bytes: &[u8], bits: u16, is_float: bool) -> f32 {
    match (bits, is_float) {
        (32, true) => f32::from_le_bytes(bytes.try_into().unwrap()),
        (8, false) => (bytes[0] as f32 - 128.0) / 128.0,
        (16, false) => i16::from_le_bytes(bytes.try_into().unwrap()) as f32 / 32768.0,
        (24, false) => {
            let v = (bytes[0] as i32) | ((bytes[1] as i32) << 8) | ((bytes[2] as i32) << 16);
            let signed = if v & 0x800000 != 0 { v | !0xFFFFFF } else { v };
            signed as f32 / 8_388_608.0
        }
        (32, false) => i32::from_le_bytes(bytes.try_into().unwrap()) as f32 / 2_147_483_648.0,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_via_temp_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("poc_com_wav_roundtrip_test.wav");
        let samples: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.02).sin() * 0.5).collect();

        write_wav_f32(&path, 48_000, &samples).expect("write should succeed");
        let (rate, read_back) = read_wav_f32(&path).expect("read should succeed");

        assert_eq!(rate, 48_000);
        assert_eq!(read_back.len(), samples.len());
        for (a, b) in samples.iter().zip(read_back.iter()) {
            assert!((a - b).abs() < 0.001, "sample mismatch: {a} vs {b}");
        }

        let _ = std::fs::remove_file(&path);
    }
}
