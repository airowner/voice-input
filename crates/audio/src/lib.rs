//! Bounded native capture. The realtime callback only copies into fixed blocks.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Sender, TrySendError, bounded};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use voice_core::AppError;

const BLOCK: usize = 2048;
const MAX_PCM: usize = 960_000;

fn error(code: &str, message: &str) -> AppError {
    AppError::new(code, "capture", true, message)
}

#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

pub fn devices() -> Result<Vec<DeviceInfo>, AppError> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|_| error("audio_devices", "无法读取麦克风列表"))?;
    Ok(devices
        .filter_map(|device| {
            Some(DeviceInfo {
                id: device.id().ok()?.to_string(),
                name: device.description().ok()?.name().to_string(),
            })
        })
        .collect())
}

pub struct Capture {
    stream: Option<cpal::Stream>,
    worker: Option<JoinHandle<Result<Vec<f32>, AppError>>>,
    cancelled: Arc<AtomicBool>,
}

impl Capture {
    pub fn start(device_id: &str, max_seconds: u32) -> Result<Self, AppError> {
        if !(1..=60).contains(&max_seconds) {
            return Err(error("audio_limit", "录音上限须在 1–60 秒内"));
        }
        let host = cpal::default_host();
        let device = if device_id == "default" {
            host.default_input_device()
        } else {
            host.input_devices().ok().and_then(|mut items| {
                items.find(|device| device.id().is_ok_and(|id| id.to_string() == device_id))
            })
        }
        .ok_or_else(|| error("microphone_missing", "所选麦克风不可用，请重新选择"))?;
        let config = device
            .default_input_config()
            .map_err(|_| error("audio_config", "无法读取麦克风采样配置"))?;
        let channels = config.channels() as usize;
        let rate = config.sample_rate();
        if channels == 0 || channels > 32 || !(8_000..=384_000).contains(&rate) {
            return Err(error("audio_config", "不支持此麦克风的采样配置"));
        }
        macro_rules! build {
            ($t:ty) => {
                Self::build::<$t>(&device, &config.into(), rate, channels, max_seconds)
            };
        }
        match config.sample_format() {
            cpal::SampleFormat::F32 => build!(f32),
            cpal::SampleFormat::F64 => build!(f64),
            cpal::SampleFormat::I8 => build!(i8),
            cpal::SampleFormat::I16 => build!(i16),
            cpal::SampleFormat::I32 => build!(i32),
            cpal::SampleFormat::I64 => build!(i64),
            cpal::SampleFormat::U8 => build!(u8),
            cpal::SampleFormat::U16 => build!(u16),
            cpal::SampleFormat::U32 => build!(u32),
            cpal::SampleFormat::U64 => build!(u64),
            _ => Err(error("audio_format", "不支持此麦克风的样本格式")),
        }
    }

    fn build<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        rate: u32,
        channels: usize,
        max_seconds: u32,
    ) -> Result<Self, AppError>
    where
        T: cpal::SizedSample + Send + Copy + 'static,
        f32: cpal::FromSample<T>,
    {
        struct Block<T> {
            values: [T; BLOCK],
            len: usize,
        }
        let (sender, receiver) = bounded::<Block<T>>(128);
        let failed = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let callback_failed = failed.clone();
        let stream_failed = failed.clone();
        let callback_cancelled = cancelled.clone();
        let callback_sender: Sender<Block<T>> = sender;
        let stream = device
            .build_input_stream::<T, _, _>(
                config,
                move |data, _| {
                    if callback_cancelled.load(Ordering::Relaxed)
                        || callback_failed.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    for slice in data.chunks(BLOCK) {
                        let mut block = Block {
                            values: [T::EQUILIBRIUM; BLOCK],
                            len: slice.len(),
                        };
                        block.values[..slice.len()].copy_from_slice(slice);
                        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
                            callback_sender.try_send(block)
                        {
                            callback_failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                },
                move |_| {
                    stream_failed.store(true, Ordering::Release);
                },
                Some(std::time::Duration::from_secs(3)),
            )
            .map_err(|_| error("audio_open", "无法打开麦克风，请检查权限与设备占用"))?;
        let worker_cancelled = cancelled.clone();
        let worker = thread::spawn(move || {
            let max_frames = rate as usize * max_seconds as usize;
            let mut mono = Vec::with_capacity(max_frames);
            let mut channel_sum = 0.0_f32;
            let mut channel_index = 0;
            for block in receiver {
                if worker_cancelled.load(Ordering::Acquire) {
                    return Err(error("cancelled", "录音已取消"));
                }
                for value in &block.values[..block.len] {
                    channel_sum += cpal::Sample::to_sample::<f32>(*value);
                    channel_index += 1;
                    if channel_index == channels {
                        if mono.len() < max_frames {
                            mono.push(channel_sum / channels as f32);
                        }
                        channel_sum = 0.0;
                        channel_index = 0;
                    }
                }
            }
            if failed.load(Ordering::Acquire) {
                return Err(error(
                    "audio_disconnected_or_overflow",
                    "采集失败或设备已断开，录音未提交",
                ));
            }
            if worker_cancelled.load(Ordering::Acquire) {
                return Err(error("cancelled", "录音已取消"));
            }
            resample(&mono, rate)
        });
        if stream.play().is_err() {
            cancelled.store(true, Ordering::Release);
            drop(stream);
            let _ = worker.join();
            return Err(error("audio_start", "麦克风无法开始录音"));
        }
        Ok(Self {
            stream: Some(stream),
            worker: Some(worker),
            cancelled,
        })
    }

    pub fn stop(mut self) -> Result<Vec<f32>, AppError> {
        self.stream.take();
        self.worker
            .take()
            .expect("capture worker")
            .join()
            .map_err(|_| error("audio_worker", "采集工作线程已退出"))?
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.stream.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn contains_voice(pcm: &[f32]) -> bool {
    if pcm.len() < 4_800 || pcm.iter().any(|v| !v.is_finite()) {
        return false;
    }
    // Conservative silence gate, not a voice activity detector.
    let rms = (pcm.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt();
    rms > 0.000_01
}

pub fn resample(mono: &[f32], rate: u32) -> Result<Vec<f32>, AppError> {
    if rate == 0 || mono.iter().any(|v| !v.is_finite()) {
        return Err(error("audio_data", "音频样本无效"));
    }
    if rate == 16_000 {
        if mono.len() > MAX_PCM {
            return Err(error("audio_limit", "音频超过 60 秒"));
        }
        return Ok(mono.to_vec());
    }
    let expected = (mono.len() as u64 * 16_000 / u64::from(rate)) as usize;
    if expected > MAX_PCM {
        return Err(error("audio_limit", "音频超过 60 秒"));
    }
    let chunk = 1024;
    let params = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut converter = SincFixedIn::<f32>::new(16_000.0 / rate as f64, 1.0, params, chunk, 1)
        .map_err(|_| error("resample", "无法初始化重采样"))?;
    // SincFixedIn starts at a centred negative input index and already aligns
    // the first output with the input. Cropping output_delay again loses audio.
    let mut output = Vec::with_capacity(expected + 4096);
    for data in mono.chunks(chunk) {
        let converted = converter
            .process_partial(Some(&[data]), None)
            .map_err(|_| error("resample", "音频重采样失败"))?;
        output.extend_from_slice(&converted[0]);
    }
    while output.len() < expected {
        let converted = converter
            .process_partial::<&[f32]>(None, None)
            .map_err(|_| error("resample", "音频重采样失败"))?;
        output.extend_from_slice(&converted[0]);
    }
    output.truncate(expected);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn silence_short_nan_and_soft_speech() {
        assert!(!contains_voice(&vec![0.0; 16000]));
        assert!(!contains_voice(&vec![0.1; 100]));
        assert!(!contains_voice(&vec![f32::NAN; 16000]));
        assert!(contains_voice(&vec![0.0001; 16000]));
    }
    #[test]
    fn resampling_preserves_duration_and_tone() {
        let source: Vec<f32> = (0..48000)
            .map(|n| (n as f32 * std::f32::consts::TAU * 440.0 / 48000.0).sin() * 0.2)
            .collect();
        let out = resample(&source, 48000).unwrap();
        assert_eq!(out.len(), 16000);
        let mse = out[200..15800]
            .iter()
            .enumerate()
            .map(|(n, v)| {
                let expected =
                    (((n + 200) as f32 * std::f32::consts::TAU * 440.0 / 16000.0).sin()) * 0.2;
                (v - expected).powi(2)
            })
            .sum::<f32>()
            / 15600.0;
        let mut impulse = vec![0.0; 48000];
        impulse[12000] = 1.0;
        let converted = resample(&impulse, 48000).unwrap();
        let peak = converted
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        assert!(peak.abs_diff(4000) <= 1, "impulse peak {peak}");
        assert!(mse < 0.001, "tone MSE {mse}");
    }
}
