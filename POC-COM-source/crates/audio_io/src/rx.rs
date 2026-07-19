use crate::device::find_input_device;
use crate::error::AudioIoError;
use crate::resample::resample;
use cpal::traits::{DeviceTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Handle to a reception running on its own dedicated OS thread. Only ever
/// captures from the input device into an internal buffer -- there is no
/// code path that wires this back out to an output device, satisfying the
/// "no audio passthrough" requirement structurally, not just by
/// convention.
pub struct RxHandle {
    buffer: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
    device_rate: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RxHandle {
    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    pub fn samples_captured(&self) -> usize {
        self.buffer.lock().expect("rx buffer lock poisoned").len()
    }

    /// A live snapshot of captured audio so far, at the device's native
    /// rate -- useful for a GUI to draw a live level/waterfall indicator
    /// while listening.
    pub fn snapshot(&self) -> Vec<f32> {
        self.buffer.lock().expect("rx buffer lock poisoned").clone()
    }

    /// Just the last `n` samples (or fewer, if less has been captured),
    /// at the device's native rate. Cheap to call every poll tick even
    /// while the underlying buffer has been growing for a long time
    /// (continuous-listen mode), unlike `snapshot()` which clones
    /// everything captured so far.
    pub fn tail(&self, n: usize) -> Vec<f32> {
        let buf = self.buffer.lock().expect("rx buffer lock poisoned");
        let start = buf.len().saturating_sub(n);
        buf[start..].to_vec()
    }

    /// Stop capturing and return the final buffer resampled to `target_rate`
    /// (the modem's internal sample rate).
    pub fn finish(mut self, target_rate: u32) -> Vec<f32> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let raw = self.buffer.lock().expect("rx buffer lock poisoned").clone();
        resample(&raw, self.device_rate, target_rate)
    }
}

pub fn start_reception(device_name: &str) -> Result<RxHandle, AudioIoError> {
    let device = find_input_device(device_name)?;
    let supported = device.default_input_config().map_err(|e| AudioIoError::UnsupportedConfig(e.to_string()))?;
    let device_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    let stream_config: cpal::StreamConfig = supported.into();

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let buffer_t = buffer.clone();
    let stop_t = stop.clone();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), AudioIoError>>();

    let join = std::thread::spawn(move || {
        let result = build_and_capture_input(&device, &stream_config, sample_format, channels, buffer_t);
        match result {
            Ok(stream) => {
                let _ = ready_tx.send(Ok(()));
                while !stop_t.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                drop(stream);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(RxHandle { buffer, stop, device_rate, join: Some(join) }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(AudioIoError::StreamPlay("audio worker thread exited before starting".into())),
    }
}

fn build_and_capture_input(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    buffer: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream, AudioIoError> {
    match format {
        cpal::SampleFormat::F32 => build_input_typed(device, config, channels, buffer, |x: f32| x),
        cpal::SampleFormat::I16 => build_input_typed(device, config, channels, buffer, |x: i16| x as f32 / i16::MAX as f32),
        cpal::SampleFormat::U16 => {
            build_input_typed(device, config, channels, buffer, |x: u16| (x as f32 / u16::MAX as f32) * 2.0 - 1.0)
        }
        other => Err(AudioIoError::UnsupportedConfig(format!("unsupported input sample format {other:?}"))),
    }
}

fn build_input_typed<T: cpal::SizedSample + Copy + Send + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    buffer: Arc<Mutex<Vec<f32>>>,
    convert: impl Fn(T) -> f32 + Send + 'static,
) -> Result<cpal::Stream, AudioIoError> {
    let stream = device
        .build_input_stream(
            config,
            move |input: &[T], _info: &cpal::InputCallbackInfo| {
                let mut buf = buffer.lock().expect("rx buffer lock poisoned");
                let ch = channels.max(1);
                for frame in input.chunks(ch) {
                    let sum: f32 = frame.iter().map(|&s| convert(s)).sum();
                    buf.push(sum / frame.len() as f32);
                }
            },
            |_err| {},
            None,
        )
        .map_err(|e| AudioIoError::StreamBuild(e.to_string()))?;
    stream.play().map_err(|e| AudioIoError::StreamPlay(e.to_string()))?;
    Ok(stream)
}
