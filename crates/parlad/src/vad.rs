//! Utterance validation and silence trimming.
//!
//! Whisper hallucinates on silence and noise, so before ASR a capture is
//! trimmed to where speech is, held to a minimum duration, and dropped when
//! no speech was found. Two gates implement that behind one call:
//!
//! - **Silero**, whisper.cpp's built-in frame-level VAD, when the model file
//!   named by `asr.vad_model_path` exists. It returns speech segments; the
//!   capture is cut to the span from the first segment's start to the last
//!   segment's end, pauses included, since whisper copes with pauses and
//!   concatenating segments would move words in time.
//! - The **energy gate**, an RMS threshold per 30 ms frame, when the model
//!   is absent or the Silero call fails on a capture.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use whisper_rs::{WhisperVadContext, WhisperVadContextParams, WhisperVadParams};

use crate::config::{AsrConfig, AudioConfig};

/// The speech gate a capture passes through before whisper.
pub enum Gate {
    Silero {
        ctx: Mutex<WhisperVadContext>,
        path: PathBuf,
        /// Speech probability at or above which a frame counts as speech.
        threshold: f32,
    },
    Energy {
        /// Why Silero is not in use, for `--check` and the log.
        reason: String,
    },
}

impl Gate {
    /// Load the Silero model once at startup, or fall back to the energy
    /// gate when the file is missing or unreadable. Never fails: a missing
    /// VAD model degrades the gate, it does not stop the daemon.
    pub fn load(asr: &AsrConfig, audio: &AudioConfig) -> Gate {
        let path = &asr.vad_model_path;
        if !path.exists() {
            let reason = format!("no model at {}, run scripts/fetch-model.sh", path.display());
            tracing::warn!("VAD: {reason}; using the energy gate");
            return Gate::Energy { reason };
        }
        match load_silero(path, asr.threads) {
            Ok(ctx) => {
                tracing::info!("VAD: silero ({})", path.display());
                Gate::Silero {
                    ctx: Mutex::new(ctx),
                    path: path.clone(),
                    threshold: audio.vad_threshold,
                }
            }
            Err(e) => {
                let reason = format!("failed to load {}: {e}", path.display());
                tracing::warn!("VAD: {reason}; using the energy gate");
                Gate::Energy { reason }
            }
        }
    }

    /// One line for `parlad --check`.
    pub fn describe(&self) -> String {
        match self {
            Gate::Silero { path, .. } => format!("silero ({})", path.display()),
            Gate::Energy { reason } => format!("energy gate ({reason})"),
        }
    }

    /// Trim, then enforce the duration policy. Ok = worth transcribing.
    pub fn validate(
        &self,
        samples: &[f32],
        rate: u32,
        cfg: &AudioConfig,
    ) -> anyhow::Result<Vec<f32>> {
        match self {
            Gate::Silero { ctx, threshold, .. } => {
                let mut ctx = ctx.lock().unwrap_or_else(PoisonError::into_inner);
                match speech_span(&mut ctx, *threshold, samples, rate, cfg) {
                    Ok(trimmed) => finish(trimmed, rate, cfg),
                    Err(SileroError::NoSpeech) => {
                        anyhow::bail!(
                            "no speech detected (peak rms {:.4})",
                            peak_rms(samples, rate)
                        )
                    }
                    Err(SileroError::Vad(e)) => {
                        tracing::warn!("VAD failed on this capture ({e}); using the energy gate");
                        validate(samples, rate, cfg)
                    }
                }
            }
            Gate::Energy { .. } => validate(samples, rate, cfg),
        }
    }
}

fn load_silero(path: &Path, threads: usize) -> anyhow::Result<WhisperVadContext> {
    // The model lines ggml logs while loading go through tracing, like
    // whisper's own. Installing twice is a no-op.
    whisper_rs::install_logging_hooks();
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))?;
    let mut params = WhisperVadContextParams::default();
    // A few hundred kilobytes of weights: the GPU would cost more to reach
    // than the CPU takes to run it.
    params.set_use_gpu(false);
    params.set_n_threads(threads.clamp(1, i32::MAX as usize) as i32);
    let t0 = std::time::Instant::now();
    let ctx = WhisperVadContext::new(path, params).map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::debug!(
        "loaded VAD model in {:.0}ms",
        t0.elapsed().as_secs_f32() * 1000.0
    );
    Ok(ctx)
}

enum SileroError {
    NoSpeech,
    Vad(whisper_rs::WhisperError),
}

/// The samples from the first speech segment's start to the last one's end,
/// with whisper.cpp's own `speech_pad_ms` already applied at both ends.
fn speech_span(
    ctx: &mut WhisperVadContext,
    threshold: f32,
    samples: &[f32],
    rate: u32,
    cfg: &AudioConfig,
) -> Result<Vec<f32>, SileroError> {
    if samples.is_empty() {
        return Err(SileroError::NoSpeech);
    }
    let mut params = WhisperVadParams::default();
    params.set_threshold(threshold);
    // whisper.cpp drops speech runs shorter than 250 ms on its own; a
    // smaller `min_utterance_ms` would be unreachable if that stayed fixed.
    params.set_min_speech_duration(cfg.min_utterance_ms.min(250) as i32);
    let segments = ctx
        .segments_from_samples(params, samples)
        .map_err(SileroError::Vad)?;
    let mut first: Option<f32> = None;
    let mut last: Option<f32> = None;
    for seg in segments {
        first = Some(first.map_or(seg.start, |f| f.min(seg.start)));
        last = Some(last.map_or(seg.end, |l| l.max(seg.end)));
    }
    let (Some(first), Some(last)) = (first, last) else {
        return Err(SileroError::NoSpeech);
    };
    // Timestamps are centiseconds.
    let per_cs = rate as f32 / 100.0;
    let start = ((first * per_cs).round().max(0.0) as usize).min(samples.len());
    let end = ((last * per_cs).round().max(0.0) as usize).clamp(start, samples.len());
    tracing::debug!(
        "VAD: speech from {:.2}s to {:.2}s of {:.2}s",
        first / 100.0,
        last / 100.0,
        samples.len() as f32 / rate as f32
    );
    if end == start {
        return Err(SileroError::NoSpeech);
    }
    Ok(samples[start..end].to_vec())
}

pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Trim leading/trailing runs below the threshold. Frame-based (30 ms) so a
/// brief dip mid-speech doesn't cut words.
pub fn trim_silence(samples: &[f32], rate: u32, threshold: f32) -> Vec<f32> {
    let frame = (rate as usize * 30 / 1000).max(1);
    let frames: Vec<f32> = samples.chunks(frame).map(rms).collect();
    let first = frames.iter().position(|&r| r >= threshold);
    let last = frames.iter().rposition(|&r| r >= threshold);
    match (first, last) {
        (Some(f), Some(l)) => {
            let start = (f * frame).min(samples.len());
            let end = ((l + 1) * frame).min(samples.len());
            samples[start..end].to_vec()
        }
        _ => Vec::new(),
    }
}

/// The energy gate: trim by RMS, then enforce duration policy.
pub fn validate(samples: &[f32], rate: u32, cfg: &AudioConfig) -> anyhow::Result<Vec<f32>> {
    let trimmed = trim_silence(samples, rate, cfg.speech_threshold);
    anyhow::ensure!(
        !trimmed.is_empty(),
        "no speech above threshold (peak rms {:.4})",
        peak_rms(samples, rate)
    );
    finish(trimmed, rate, cfg)
}

/// The duration policy both gates share: drop a trimmed capture shorter
/// than `min_utterance_ms`, keep at most the first `max_utterance_ms`.
fn finish(trimmed: Vec<f32>, rate: u32, cfg: &AudioConfig) -> anyhow::Result<Vec<f32>> {
    let ms = trimmed.len() as u64 * 1000 / rate as u64;
    anyhow::ensure!(
        ms >= cfg.min_utterance_ms,
        "utterance too short ({ms} ms < {} ms)",
        cfg.min_utterance_ms
    );
    let max_samples = (rate as usize / 1000) * cfg.max_utterance_ms as usize;
    Ok(if trimmed.len() > max_samples {
        trimmed[..max_samples].to_vec()
    } else {
        trimmed
    })
}

fn peak_rms(samples: &[f32], rate: u32) -> f32 {
    let frame = (rate as usize * 30 / 1000).max(1);
    samples.chunks(frame).map(rms).fold(0.0f32, |a, b| a.max(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AudioConfig {
        AudioConfig::default()
    }

    #[test]
    fn trims_silence_and_validates() {
        let rate: u32 = 16_000;
        let n = rate as usize;
        let mut s = vec![0.0f32; n / 2]; // 0.5 s silence
        s.extend(vec![0.3f32; n]); // 1 s "speech"
        s.extend(vec![0.0f32; n / 2]); // 0.5 s silence
        let out = validate(&s, rate, &cfg()).unwrap();
        let ms = out.len() * 1000 / n;
        assert!((900..1100).contains(&ms), "expected ~1000ms, got {ms}");
    }

    #[test]
    fn rejects_silence() {
        let s = vec![0.0001f32; 16_000];
        assert!(validate(&s, 16_000, &cfg()).is_err());
    }

    #[test]
    fn rejects_too_short() {
        let s = vec![0.3f32; 1600]; // 100 ms
        assert!(validate(&s, 16_000, &cfg()).is_err());
    }

    #[test]
    fn energy_gate_through_the_gate_type() {
        let gate = Gate::Energy {
            reason: "test".into(),
        };
        let s = vec![0.3f32; 16_000];
        assert_eq!(gate.validate(&s, 16_000, &cfg()).unwrap().len(), 16_000);
        assert!(gate.describe().starts_with("energy gate ("));
    }

    #[test]
    fn missing_model_falls_back_to_energy() {
        let asr = AsrConfig {
            vad_model_path: PathBuf::from("/nonexistent/parla-test/ggml-silero.bin"),
            ..AsrConfig::default()
        };
        let gate = Gate::load(&asr, &cfg());
        assert!(matches!(gate, Gate::Energy { .. }), "{}", gate.describe());
        assert!(gate
            .describe()
            .contains("/nonexistent/parla-test/ggml-silero.bin"));
    }

    /// Needs the model at the default path (scripts/fetch-model.sh vad);
    /// skipped otherwise. No speech sample ships with the repo, so this
    /// covers loading, that silence yields "no speech", and that a Silero
    /// failure cannot poison the gate for the next capture.
    #[test]
    fn silero_loads_and_rejects_silence() {
        let asr = AsrConfig::default();
        if !asr.vad_model_path.exists() {
            eprintln!(
                "skipping: no VAD model at {} (run scripts/fetch-model.sh vad)",
                asr.vad_model_path.display()
            );
            return;
        }
        let gate = Gate::load(&asr, &cfg());
        assert!(matches!(gate, Gate::Silero { .. }), "{}", gate.describe());
        assert!(gate.describe().starts_with("silero ("));

        // Pure silence, and a DC-only "tone" the energy gate would accept.
        let silence = vec![0.0f32; 16_000 * 2];
        let err = gate.validate(&silence, 16_000, &cfg()).unwrap_err();
        assert!(err.to_string().contains("no speech"), "{err}");
        let flat = vec![0.3f32; 16_000];
        let err = gate.validate(&flat, 16_000, &cfg()).unwrap_err();
        assert!(err.to_string().contains("no speech"), "{err}");

        // An empty capture is refused without reaching the model.
        assert!(gate.validate(&[], 16_000, &cfg()).is_err());
    }

    /// Real speech through Silero. Needs the model and a recording, since
    /// no synthetic signal reliably reads as speech: point
    /// `PARLA_VAD_SPEECH_RAW` at raw little-endian f32 samples, 16 kHz
    /// mono, with silence around the words. One way to make one:
    /// `ffmpeg -i /usr/share/sounds/alsa/Front_Center.wav -ac 1 -ar 16000 -f f32le speech.f32`
    #[test]
    fn silero_finds_speech_in_a_recording() {
        let asr = AsrConfig::default();
        let Ok(raw) = std::env::var("PARLA_VAD_SPEECH_RAW") else {
            eprintln!("skipping: PARLA_VAD_SPEECH_RAW not set");
            return;
        };
        if !asr.vad_model_path.exists() {
            eprintln!("skipping: no VAD model at {}", asr.vad_model_path.display());
            return;
        }
        let bytes = std::fs::read(&raw).unwrap();
        let (chunks, _) = bytes.as_chunks::<4>();
        let samples: Vec<f32> = chunks.iter().map(|b| f32::from_le_bytes(*b)).collect();
        // 1 s of silence on each side, so there is something to trim.
        let mut padded = vec![0.0f32; 16_000];
        padded.extend_from_slice(&samples);
        padded.extend(vec![0.0f32; 16_000]);

        let gate = Gate::load(&asr, &cfg());
        assert!(matches!(gate, Gate::Silero { .. }), "{}", gate.describe());
        let out = gate.validate(&padded, 16_000, &cfg()).unwrap();
        let ms = out.len() * 1000 / 16_000;
        let total_ms = padded.len() * 1000 / 16_000;
        let speech_ms = samples.len() * 1000 / 16_000;
        eprintln!("kept {ms} ms of {total_ms} ms (recording is {speech_ms} ms)");
        // Shorter than the whole capture, so the padding was trimmed, and
        // not shorter than the recording by more than a leading and
        // trailing breath: the span must land on the words.
        assert!(ms < total_ms - 1_000, "kept {ms} ms of {total_ms} ms");
        assert!(
            ms + 600 >= speech_ms,
            "kept {ms} ms of a {speech_ms} ms recording"
        );
        // The kept audio is a slice of the recording, not something else.
        assert!(out.iter().any(|s| s.abs() > 0.1), "kept slice is silent");
    }
}
