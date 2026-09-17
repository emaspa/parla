//! ASR: whisper.cpp (CUDA build) via whisper-rs, batch-on-end-of-speech.
//! Utterances here are 1–10 s; large-v3-turbo on a desktop GPU returns in a
//! few hundred ms (plan §2).

use std::path::Path;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::audio::SAMPLE_RATE;
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
        let audio_ms = samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
        tracing::info!(
            "transcribed {audio_ms}ms audio in {:.0}ms: {:?}",
            elapsed.as_secs_f32() * 1000.0,
            text.trim()
        );

        Ok(filter(&self.blocklist, &text))
    }
}

/// Normalize whitespace and drop known silence-hallucinations: a transcript
/// that is nothing but a blocklisted phrase is dropped, and a blocklisted
/// phrase whisper tacked onto the end of real speech is stripped. Matching
/// is case-insensitive and on word boundaries, so "I love you" survives a
/// "you" entry and "hello thank you" becomes "hello".
fn filter(blocklist: &[String], text: &str) -> String {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let core = cleaned.trim_matches(|c: char| !c.is_alphanumeric());
    if core.is_empty() {
        return String::new();
    }
    let core_lower = core.to_lowercase();
    let entries: Vec<String> = blocklist
        .iter()
        .map(|b| b.trim().to_lowercase())
        .filter(|b| !b.is_empty())
        .collect();
    if entries.contains(&core_lower) {
        tracing::debug!("dropped hallucination: {cleaned:?}");
        return String::new();
    }
    // whisper's classic filler on near-silence, at the tail of real speech
    for b in &entries {
        if let Some(head) = strip_suffix_word(core, b) {
            tracing::debug!("stripped trailing hallucination {b:?} from {cleaned:?}");
            return head.to_string();
        }
    }
    core.to_string()
}

/// `text` minus a trailing `suffix` (already lowercase) when it is its own
/// word(s): preceded by whitespace and not the whole text. Works in chars,
/// never in the byte length of a lowercased copy, since lowercasing can
/// change a string's byte length.
fn strip_suffix_word<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let n = suffix.chars().count();
    let (idx, _) = text.char_indices().rev().nth(n - 1)?;
    if text[idx..].to_lowercase() != suffix {
        return None;
    }
    let head = &text[..idx];
    let head_trimmed = head.trim_end();
    if head_trimmed.len() == head.len() || head_trimmed.is_empty() {
        return None;
    }
    Some(head_trimmed)
}

#[cfg(test)]
mod tests {
    use super::filter;

    fn blocklist() -> Vec<String> {
        crate::config::AsrConfig::default().hallucination_blocklist
    }

    #[test]
    fn real_speech_survives() {
        assert_eq!(filter(&blocklist(), "I love you"), "I love you");
        assert_eq!(filter(&blocklist(), "  open   firefox "), "open firefox");
    }

    #[test]
    fn lone_hallucination_is_dropped() {
        assert_eq!(filter(&blocklist(), "thank you"), "");
        assert_eq!(filter(&blocklist(), " Thank you. "), "");
        assert_eq!(filter(&blocklist(), "..."), "");
    }

    #[test]
    fn trailing_hallucination_is_stripped_on_word_boundary() {
        assert_eq!(filter(&blocklist(), "hello thank you"), "hello");
        assert_eq!(filter(&blocklist(), "hello Thank You."), "hello");
        // not a word boundary: leave it alone
        assert_eq!(filter(&blocklist(), "hellothank you"), "hellothank you");
    }

    #[test]
    fn non_ascii_before_suffix_does_not_panic() {
        assert_eq!(
            filter(&blocklist(), "ciao è tutto thank you"),
            "ciao è tutto"
        );
        assert_eq!(
            filter(&blocklist(), "İstanbul İzmir thank you"),
            "İstanbul İzmir"
        );
        assert_eq!(filter(&blocklist(), "日本語 the end"), "日本語");
        // suffix longer than the text
        assert_eq!(filter(&["thanks for watching".into()], "é"), "é");
    }
}
