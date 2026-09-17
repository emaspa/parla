//! Microphone capture via cpal (ALSA/PipeWire), downmixed to mono and
//! resampled to the 16 kHz whisper expects.
//!
//! PTT records the whole utterance; silence trimming and validation happen
//! afterwards in `vad`. Streaming VAD can slot in later behind the same
//! interface without changing callers.
//!
//! The cpal stream lives on its own thread: opening a PipeWire device can
//! block for a while, and `cpal::Stream` is not `Send`, so the runtime only
//! ever holds a handle that talks to that thread over channels.

use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context as _;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::oneshot;

/// Whisper models are trained on 16 kHz audio; everything is resampled to it.
pub const SAMPLE_RATE: u32 = 16_000;

type Buffer = Arc<Mutex<Vec<f32>>>;

/// Handle to a running capture. Dropping it stops the capture thread.
pub struct CaptureSession {
    /// Dropped (or fired) to stop the capture thread.
    stop_tx: oneshot::Sender<()>,
    done_rx: oneshot::Receiver<anyhow::Result<Vec<f32>>>,
    /// Resolves once the device is open (with its name) or failed to open.
    ready_rx: Option<oneshot::Receiver<anyhow::Result<String>>>,
}

/// A capture that has been told to stop; resolves to the recorded samples.
pub struct Stopped {
    done_rx: oneshot::Receiver<anyhow::Result<Vec<f32>>>,
}

impl std::future::Future for Stopped {
    type Output = anyhow::Result<Vec<f32>>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.done_rx).poll(cx).map(|r| {
            r.context("capture thread died before returning samples")
                .and_then(|r| r)
        })
    }
}

impl CaptureSession {
    /// Start a capture thread that opens the device and records until
    /// [`stop`](Self::stop). Returns immediately; poll [`ready`](Self::ready)
    /// to learn whether the device opened. The buffer stops growing at
    /// `max_samples` (at [`SAMPLE_RATE`]).
    pub fn start(device_name: Option<String>, max_samples: usize) -> anyhow::Result<Self> {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (done_tx, done_rx) = oneshot::channel();
        let buf: Buffer = Arc::new(Mutex::new(Vec::new()));
        std::thread::Builder::new()
            .name("parla-capture".into())
            .spawn(move || {
                let stream = match open(device_name.as_deref(), max_samples, &buf) {
                    Ok((stream, name)) => {
                        let _ = ready_tx.send(Ok(name));
                        stream
                    }
                    Err(e) => {
                        let _ = done_tx.send(Err(anyhow::anyhow!(
                            "no audio: the input device failed to open ({e:#})"
                        )));
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                // Blocks until stop() or until the handle is dropped.
                let _ = stop_rx.blocking_recv();
                drop(stream);
                let _ = done_tx.send(Ok(take(&buf)));
            })
            .context("spawning capture thread")?;
        Ok(Self {
            stop_tx,
            done_rx,
            ready_rx: Some(ready_rx),
        })
    }

    /// Resolves once, to the device name or the open error. Later calls
    /// pend forever, so it is safe to poll in a `select!` loop.
    pub async fn ready(&mut self) -> anyhow::Result<String> {
        match self.ready_rx.take() {
            Some(rx) => rx.await.context("capture thread died while opening")?,
            None => std::future::pending().await,
        }
    }

    /// Stop recording now; the returned future yields the mono 16 kHz
    /// samples once the capture thread hands them over.
    pub fn stop(self) -> Stopped {
        let Self {
            stop_tx, done_rx, ..
        } = self;
        let _ = stop_tx.send(());
        Stopped { done_rx }
    }
}

fn take(buffer: &Buffer) -> Vec<f32> {
    std::mem::take(&mut *buffer.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Open the device and start the stream; runs on the capture thread.
fn open(
    device_name: Option<&str>,
    max_samples: usize,
    buffer: &Buffer,
) -> anyhow::Result<(cpal::Stream, String)> {
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
    anyhow::ensure!(src_rate > 0, "device {name} reports a 0 Hz sample rate");

    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => build_stream::<f32, _>(
            &device,
            &cfg,
            src_rate,
            max_samples,
            channels,
            buffer,
            |s| s,
        )?,
        cpal::SampleFormat::I16 => build_stream::<i16, _>(
            &device,
            &cfg,
            src_rate,
            max_samples,
            channels,
            buffer,
            |s| s as f32 / i16::MAX as f32,
        )?,
        cpal::SampleFormat::I32 => build_stream::<i32, _>(
            &device,
            &cfg,
            src_rate,
            max_samples,
            channels,
            buffer,
            |s| s as f32 / i32::MAX as f32,
        )?,
        cpal::SampleFormat::I64 => build_stream::<i64, _>(
            &device,
            &cfg,
            src_rate,
            max_samples,
            channels,
            buffer,
            |s| s as f32 / i64::MAX as f32,
        )?,
        cpal::SampleFormat::U8 => build_stream::<u8, _>(
            &device,
            &cfg,
            src_rate,
            max_samples,
            channels,
            buffer,
            |s| (s as f32 - 128.0) / 128.0,
        )?,
        cpal::SampleFormat::U16 => build_stream::<u16, _>(
            &device,
            &cfg,
            src_rate,
            max_samples,
            channels,
            buffer,
            |s| (s as f32 - 32768.0) / 32768.0,
        )?,
        other => anyhow::bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    Ok((stream, name))
}

fn build_stream<T, C>(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    src_rate: u32,
    max_samples: usize,
    channels: usize,
    buffer: &Buffer,
    convert: C,
) -> anyhow::Result<cpal::Stream>
where
    T: cpal::SizedSample,
    C: Fn(T) -> f32 + Send + 'static,
{
    let stream_cfg: cpal::StreamConfig = cfg.clone().into();
    let buf = Arc::clone(buffer);
    let err_fn = |e| tracing::error!("cpal stream error: {e}");
    let mut resampler = Resampler::new(src_rate, SAMPLE_RATE);
    let mut capped = false;
    let stream = device.build_input_stream::<T, _, _>(
        &stream_cfg,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mono_f32: Vec<f32> = downmix(data, channels, &convert);
            let resampled = resampler.push(&mono_f32);
            let mut buf = buf.lock().unwrap_or_else(PoisonError::into_inner);
            let room = max_samples.saturating_sub(buf.len());
            if room < resampled.len() && !capped {
                capped = true;
                tracing::warn!("capture buffer full ({max_samples} samples); dropping the rest");
            }
            buf.extend_from_slice(&resampled[..room.min(resampled.len())]);
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

/// Box-average resampler that carries its phase across calls, so feeding it
/// audio in arbitrary chunks yields exactly what one pass over the whole
/// signal would. Decent anti-alias for the integer ratios audio interfaces
/// produce (48k -> 16k); 44.1k -> 16k is fractional but fine for speech.
///
/// Output sample `i` is the mean of input samples
/// `floor(i * ratio) .. floor((i + 1) * ratio)`, indexed from the start of
/// the stream.
struct Resampler {
    ratio: f64,
    /// Input samples not yet consumed by a complete output window.
    carry: Vec<f32>,
    /// Absolute input index of `carry[0]`.
    carry_start: u64,
    /// Next output index.
    out_idx: u64,
}

impl Resampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            ratio: f64::from(src_rate) / f64::from(dst_rate),
            carry: Vec::new(),
            carry_start: 0,
            out_idx: 0,
        }
    }

    fn push(&mut self, input: &[f32]) -> Vec<f32> {
        if self.ratio == 1.0 {
            return input.to_vec();
        }
        self.carry.extend_from_slice(input);
        let mut out = Vec::new();
        loop {
            let start = self.window_start(self.out_idx);
            let end = self.window_start(self.out_idx + 1).max(start + 1);
            let available = self.carry_start + self.carry.len() as u64;
            if end > available {
                break;
            }
            let s = (start - self.carry_start) as usize;
            let e = (end - self.carry_start) as usize;
            let sum: f32 = self.carry[s..e].iter().sum();
            out.push(sum / (e - s) as f32);
            self.out_idx += 1;
        }
        // Drop everything before the next window so `carry` stays bounded.
        let next = self.window_start(self.out_idx);
        let drop_n = (next.saturating_sub(self.carry_start) as usize).min(self.carry.len());
        if drop_n > 0 {
            self.carry.drain(..drop_n);
            self.carry_start += drop_n as u64;
        }
        out
    }

    fn window_start(&self, out_idx: u64) -> u64 {
        (out_idx as f64 * self.ratio).floor() as u64
    }
}

/// Resample a whole signal in one pass (tests and offline use).
#[cfg(test)]
fn resample(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    Resampler::new(src_rate, dst_rate).push(input)
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
    use super::{resample, Resampler};

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

    fn signal(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| ((i as f32) * 0.01).sin() * 0.7 + ((i as f32) * 0.37).cos() * 0.2)
            .collect()
    }

    #[test]
    fn chunked_matches_whole_pass() {
        for (src, dst) in [
            (48_000, 16_000),
            (44_100, 16_000),
            (96_000, 16_000),
            (22_050, 16_000),
        ] {
            let input = signal(20_000);
            let whole = resample(&input, src, dst);

            // Odd chunk sizes so window boundaries fall inside chunks.
            for chunk in [1usize, 7, 100, 333, 1024, 4801] {
                let mut r = Resampler::new(src, dst);
                let mut chunked = Vec::new();
                for c in input.chunks(chunk) {
                    chunked.extend(r.push(c));
                }
                assert_eq!(
                    chunked, whole,
                    "src={src} dst={dst} chunk={chunk}: chunked resampling differs"
                );
            }
        }
    }

    #[test]
    fn carry_stays_bounded() {
        let mut r = Resampler::new(48_000, 16_000);
        for c in signal(48_000).chunks(480) {
            r.push(c);
            assert!(r.carry.len() < 8, "carry grew to {}", r.carry.len());
        }
    }
}
