//! ASR: whisper.cpp (CUDA build) via whisper-rs, batch-on-end-of-speech.
//! Utterances here are 1–10 s; large-v3-turbo on a desktop GPU returns in a
//! few hundred ms (plan §2).

use std::path::Path;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::config::AsrConfig;

pub struct Asr {
    ctx: WhisperContext,
    threads: i32,
    language: String,
    initial_prompt: Option<String>,
    blocklist: Vec<String>,
}

impl Asr {
    /// Load the model. Expensive (GBs into VRAM) — call once at startup,
    /// then share via Arc; WhisperContext is Send+Sync and states are
    /// per-call.
    pub fn load(cfg: &AsrConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            cfg.model_path.exists(),
            "whisper model not found at {} (fetch it: scripts/fetch-model.sh)",
            cfg.model_path.display()
        );
        let mut ctx_params = WhisperContextParameters::default();
        ctx_params.use_gpu(true);
        let t0 = std::time::Instant::now();
        let ctx = WhisperContext::new_with_params(Path::new(&cfg.model_path), ctx_params)
            .map_err(|e| anyhow::anyhow!("failed to load whisper model: {e}"))?;
        tracing::info!("loaded whisper model in {:.1}s", t0.elapsed().as_secs_f32());
        Ok(Self {
            ctx,
            threads: cfg.threads as i32,
            language: cfg.language.clone(),
            initial_prompt: cfg.initial_prompt.clone(),
            blocklist: cfg.hallucination_blocklist.clone(),
        })
    }

    /// Transcribe mono 16 kHz f32 samples. Returns cleaned text; empty
    /// string means "nothing worth transcribing" (silence hallucination or
    /// blank output).
    pub fn transcribe(&self, samples: &[f32]) -> anyhow::Result<String> {
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_n_threads(self.threads);
        params.set_translate(false);
        params.set_language(if self.language == "auto" {
            None
        } else {
            Some(&self.language)
        });
        if let Some(prompt) = &self.initial_prompt {
            params.set_initial_prompt(prompt);
        }

        let t0 = std::time::Instant::now();
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| anyhow::anyhow!("whisper state: {e}"))?;
        state
            .full(params, samples)
            .map_err(|e| anyhow::anyhow!("whisper full: {e}"))?;
        let mut text = String::new();
        for segment in state.as_iter() {
            match segment.to_str_lossy() {
                Ok(s) => text.push_str(&s),
                Err(e) => tracing::warn!("whisper segment text error: {e}"),
            }
        }
        let elapsed = t0.elapsed();
        let audio_ms = samples.len() / 16; // 16 kHz -> ms
        tracing::info!(
            "transcribed {audio_ms}ms audio in {:.0}ms: {:?}",
            elapsed.as_secs_f32() * 1000.0,
            text.trim()
        );

        Ok(self.filter(text))
    }

    /// Normalize whitespace and drop known silence-hallucinations.
    fn filter(&self, text: String) -> String {
        let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let lower = cleaned.to_lowercase();
        let trimmed = lower
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        if trimmed.is_empty() {
            return String::new();
        }
        // exact matches or very short transcripts fully inside the blocklist
        if self
            .blocklist
            .iter()
            .any(|b| trimmed == b.trim().to_lowercase())
        {
            tracing::debug!("dropped hallucination: {cleaned:?}");
            return String::new();
        }
        // whisper's classic filler on near-silence, at the tail of real speech
        let mut out = cleaned.clone();
        for b in &self.blocklist {
            let suffix = format!(" {}", b.to_lowercase());
            if out.to_lowercase().ends_with(&suffix) && out.len() > suffix.len() + 2 {
                out.truncate(out.len() - suffix.len());
                break;
            }
        }
        out.trim().to_string()
    }
}
