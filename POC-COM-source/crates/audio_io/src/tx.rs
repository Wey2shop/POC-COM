use crate::device::find_output_device;
use crate::error::AudioIoError;
use crate::resample::resample;
use cpal::traits::{DeviceTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Handle to a transmission running on its own dedicated OS thread (cpal
/// streams aren't Send, so the stream lives entirely on that thread; this
/// handle only exposes Send+Sync atomics for the GUI to poll).
pub struct TxHandle {
    pub total_samples: usize,
    played: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    _join: std::thread::JoinHandle<()>,
}

impl TxHandle {
    pub fn samples_played(&self) -> usize {
        self.played.load(Ordering::Relaxed).min(self.total_samples)
    }

    pub fn progress(&self) -> f32 {
        if self.total_samples == 0 {
            1.0
        } else {
            self.samples_played() as f32 / self.total_samples as f32
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    /// Stop playback early. There is deliberately no gain/volume control
    /// exposed anywhere in this API -- only start/monitor/cancel.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Start playing `samples` (already peak-normalized by `hsm_modem`, at
/// `source_rate`) out `device_name` at a fixed level -- no gain is applied
/// here or anywhere else in this crate. Returns immediately with a handle
/// to poll progress; playback runs on its own thread until finished or
/// cancelled.
pub fn start_transmission(device_name: &str, samples: Vec<f32>, source_rate: u32) -> Result<TxHandle, AudioIoError> {
    let device = find_output_device(device_name)?;
    let supported = device.default_output_config().map_err(|e| AudioIoError::UnsupportedConfig(e.to_string()))?;
    let device_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    let stream_config: cpal::StreamConfig = supported.into();

    let data = resample(&samples, source_rate, device_rate);
    let total_samples = data.len();

    let played = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));

    let played_t = played.clone();
    let finished_t = finished.clone();
    let cancel_t = cancel.clone();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), AudioIoError>>();

    let join = std::thread::spawn(move || {
        let cursor = Arc::new(AtomicUsize::new(0));
        let result = build_and_play_output(&device, &stream_config, sample_format, channels, data, cursor.clone(), played_t.clone());
        match result {
            Ok(stream) => {
                let _ = ready_tx.send(Ok(()));
                loop {
                    if cancel_t.load(Ordering::Relaxed) || cursor.load(Ordering::Relaxed) >= total_samples {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                drop(stream);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
        finished_t.store(true, Ordering::Relaxed);
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(TxHandle { total_samples, played, finished, cancel, _join: join }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AudioIoError::StreamPlay("audio worker thread exited before starting".into())),
    }
}

fn build_and_play_output(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    data: Vec<f32>,
    cursor: Arc<AtomicUsize>,
    played: Arc<AtomicUsize>,
) -> Result<cpal::Stream, AudioIoError> {
    match format {
        cpal::SampleFormat::F32 => build_output_typed(device, config, channels, data, cursor, played, |x| x),
        cpal::SampleFormat::I16 => {
            build_output_typed(device, config, channels, data, cursor, played, |x| (x.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        }
        cpal::SampleFormat::U16 => build_output_typed(device, config, channels, data, cursor, played, |x| {
            (((x.clamp(-1.0, 1.0) + 1.0) * 0.5) * u16::MAX as f32) as u16
        }),
        other => Err(AudioIoError::UnsupportedConfig(format!("unsupported output sample format {other:?}"))),
    }
}

fn build_output_typed<T: cpal::SizedSample + Send + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    data: Vec<f32>,
    cursor: Arc<AtomicUsize>,
    played: Arc<AtomicUsize>,
    convert: impl Fn(f32) -> T + Send + 'static,
) -> Result<cpal::Stream, AudioIoError> {
    let total = data.len();
    let stream = device
        .build_output_stream(
            config,
            move |output: &mut [T], _info: &cpal::OutputCallbackInfo| {
                for frame in output.chunks_mut(channels.max(1)) {
                    let idx = cursor.fetch_add(1, Ordering::Relaxed);
                    let sample_f32 = data.get(idx).copied().unwrap_or(0.0);
                    let sample_t = convert(sample_f32);
                    for out in frame.iter_mut() {
                        *out = sample_t;
                    }
                    if idx + 1 <= total {
                        played.store((idx + 1).min(total), Ordering::Relaxed);
                    }
                }
            },
            |_err| {},
            None,
        )
        .map_err(|e| AudioIoError::StreamBuild(e.to_string()))?;
    stream.play().map_err(|e| AudioIoError::StreamPlay(e.to_string()))?;
    Ok(stream)
}
