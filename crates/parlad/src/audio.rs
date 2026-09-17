//! Microphone capture via cpal (ALSA/PipeWire), downmixed to mono and
//! resampled to the 16 kHz whisper expects.
//!
//! PTT records the whole utterance; silence trimming and validation happen
//! afterwards in `vad`. Streaming VAD (Silero) can slot in later behind the
//! same interface without changing callers.

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub struct CaptureSession {
    /// Held: dropping the stream stops capture.
    stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<f32>>>,
    device_name: String,
}

impl CaptureSession {
    /// Open the input device and start recording immediately.
    pub fn start(device_name: Option<&str>, target_rate: u32) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = match device_name {
            Some(name) => host
                .input_devices()?
                .find(|d| d.name().map(|n| n.contains(name)).unwrap_or(false))
                .ok_or_else(|| anyhow::anyhow!("input device {name:?} not found"))?,
            None => host
                .default_input_device()
                .ok_or_else(|| anyhow::anyhow!("no default input device"))?,
        };
        let name = device.name().unwrap_or_else(|_| "?".into());
        let cfg = device.default_input_config()?;
        let src_rate = cfg.sample_rate().0;
        let channels = cfg.channels() as usize;
        tracing::debug!(
            "capture: {name} {src_rate}Hz {channels}ch {:?}",
            cfg.sample_format()
        );

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => build_stream::<f32, _>(
                &device,
                &cfg,
                src_rate,
                target_rate,
                channels,
                &buffer,
                |s| s,
            )?,
            cpal::SampleFormat::I16 => build_stream::<i16, _>(
                &device,
                &cfg,
                src_rate,
                target_rate,
                channels,
                &buffer,
                |s| s as f32 / i16::MAX as f32,
            )?,
            cpal::SampleFormat::I32 => build_stream::<i32, _>(
                &device,
                &cfg,
                src_rate,
                target_rate,
                channels,
                &buffer,
                |s| s as f32 / i32::MAX as f32,
            )?,
            cpal::SampleFormat::I64 => build_stream::<i64, _>(
                &device,
                &cfg,
                src_rate,
                target_rate,
                channels,
                &buffer,
                |s| s as f32 / i64::MAX as f32,
            )?,
            cpal::SampleFormat::U8 => build_stream::<u8, _>(
                &device,
                &cfg,
                src_rate,
                target_rate,
                channels,
                &buffer,
                |s| (s as f32 - 128.0) / 128.0,
            )?,
            cpal::SampleFormat::U16 => build_stream::<u16, _>(
                &device,
                &cfg,
                src_rate,
                target_rate,
                channels,
                &buffer,
                |s| (s as f32 - 32768.0) / 32768.0,
            )?,
            other => anyhow::bail!("unsupported sample format {other:?}"),
        };
        stream.play()?;
        Ok(Self {
            stream,
            buffer,
            device_name: name,
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Stop recording and return the captured mono 16 kHz samples.
    pub fn stop(self) -> Vec<f32> {
        drop(self.stream);
        std::mem::take(&mut *self.buffer.lock().unwrap())
    }

    pub fn elapsed_samples(&self) -> usize {
        self.buffer.lock().unwrap().len()
    }
}

fn build_stream<T, C>(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    src_rate: u32,
    dst_rate: u32,
    channels: usize,
    buffer: &Arc<Mutex<Vec<f32>>>,
    convert: C,
) -> anyhow::Result<cpal::Stream>
where
    T: cpal::SizedSample,
    C: Fn(T) -> f32 + Send + 'static,
{
    let stream_cfg: cpal::StreamConfig = cfg.clone().into();
    let buf = Arc::clone(buffer);
    let err_fn = |e| tracing::error!("cpal stream error: {e}");
    let stream = device.build_input_stream::<T, _, _>(
        &stream_cfg,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mono_f32: Vec<f32> = downmix(data, channels, &convert);
            let resampled = resample(&mono_f32, src_rate, dst_rate);
            buf.lock().unwrap().extend_from_slice(&resampled);
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

/// Average interleaved frames down to mono f32.
fn downmix<T>(data: &[T], channels: usize, convert: &impl Fn(T) -> f32) -> Vec<f32>
where
    T: cpal::Sample,
{
    let ch = channels.max(1);
    data.chunks_exact(ch)
        .map(|frame| frame.iter().map(|&s| convert(s)).sum::<f32>() / ch as f32)
        .collect()
}

/// Box-average resample (decent anti-alias for the integer ratios audio
/// interfaces produce: 48k -> 16k, 44.1k -> 16k is fractional but fine for
/// speech).
fn resample(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = (input.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let start = (i as f64 * ratio).floor() as usize;
        let end = (((i + 1) as f64 * ratio).floor() as usize)
            .min(input.len())
            .max(start + 1);
        let sum: f32 = input[start..end].iter().sum();
        out.push(sum / (end - start) as f32);
    }
    out
}

/// List input devices (for `parlad --check`).
pub fn list_input_devices() -> anyhow::Result<Vec<String>> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    for d in host.input_devices()? {
        out.push(d.name().unwrap_or_else(|_| "<unnamed>".into()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::resample;

    #[test]
    fn resample_48_to_16() {
        let input: Vec<f32> = (0..4800).map(|i| (i % 4800) as f32).collect();
        let out = resample(&input, 48_000, 16_000);
        assert_eq!(out.len(), 1600);
        // first output = average of first 3 input samples
        assert_eq!(out[0], (0.0 + 1.0 + 2.0) / 3.0);
    }

    #[test]
    fn resample_passthrough() {
        let input = vec![1.0f32, 2.0, 3.0];
        assert_eq!(resample(&input, 16_000, 16_000), input);
    }
}
