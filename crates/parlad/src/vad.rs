//! Utterance validation and silence trimming (energy gate).
//!
//! Whisper hallucinates on silence/noise (plan §5), so before ASR we trim
//! leading/trailing silence, enforce a minimum duration, and drop captures
//! that never rose above the speech threshold. A frame-level Silero VAD can
//! replace the energy gate later without touching callers.

use crate::config::AudioConfig;

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
    let frames: Vec<f32> = samples.chunks(frame).map(|c| rms(c)).collect();
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

/// Trim, then enforce duration/energy policy. Ok = worth transcribing.
pub fn validate(samples: &[f32], rate: u32, cfg: &AudioConfig) -> anyhow::Result<Vec<f32>> {
    let trimmed = trim_silence(samples, rate, cfg.speech_threshold);
    let ms = trimmed.len() as u64 * 1000 / rate as u64;
    anyhow::ensure!(
        !trimmed.is_empty(),
        "no speech above threshold (peak rms {:.4})",
        peak_rms(samples, rate)
    );
    anyhow::ensure!(
        ms >= cfg.min_utterance_ms,
        "utterance too short ({ms} ms < {} ms)",
        cfg.min_utterance_ms
    );
    // hard cap: keep only the first max_utterance_ms
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
}
