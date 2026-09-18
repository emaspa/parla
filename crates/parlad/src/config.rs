use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A string that must never reach a log or a serialized config: the API key.
/// `Debug` and `Serialize` both redact it; read it with [`Secret::expose`].
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "Secret(<empty>)"
        } else {
            "Secret(<redacted>)"
        })
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(if self.0.is_empty() { "" } else { "<redacted>" })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub hotkeys: HotkeyConfig,
    pub audio: AudioConfig,
    pub asr: AsrConfig,
    pub router: RouterConfig,
    /// The GGUF model both the judged path and dictation cleanup share.
    pub local: LocalConfig,
    pub judge: JudgeConfig,
    pub flow: FlowConfig,
    pub desktopd: desktopd::DesktopdConfig,
}

/// The judged path: what happens to utterances the grammar rejects.
/// `backend = "local"` uses the model under `[local]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JudgeConfig {
    /// Judge utterances the fast-path grammar does not match.
    pub enabled: bool,
    /// Who answers: the model under `[local]`, or the TypeSafe API.
    pub backend: Backend,
    /// Give up rather than keep the user waiting on a voice command.
    pub timeout_ms: u64,
    /// Below this intent confidence, act on nothing. Absent means the
    /// backend's measured default; see [`JudgeConfig::thresholds`].
    pub min_confidence: Option<f64>,
    /// At or above this, act without a confirmation prompt — unless the action
    /// is judged destructive, which always prompts.
    pub act_unconfirmed_above: Option<f64>,
    /// `is_dictation` at or above this means the user held the wrong hotkey.
    pub dictation_threshold: Option<f64>,
    /// `is_destructive` at or above this forces spoken confirmation.
    pub destructive_threshold: Option<f64>,
    /// Include window titles in what the judged path sends off the machine.
    /// Only the `typesafe` backend consults this: titles carry document
    /// names, URLs and chat subjects, so by default the request names only
    /// each window's application and an index, and the title is mapped back
    /// here. The local backend always sees titles; nothing leaves the host.
    pub send_window_titles: bool,
    pub typesafe: TypeSafeConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// A GGUF model loaded through llama.cpp on this machine.
    Local,
    /// The TypeSafe System One API, over HTTPS.
    TypeSafe,
}

/// The four probabilities the policy compares against, with every value
/// resolved: what the config said, or the backend's default where it said
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Below this confidence, act on nothing.
    pub min_confidence: f64,
    /// At or above this, act without a confirmation prompt.
    pub act_unconfirmed_above: f64,
    /// `is_dictation` at or above this means the user held the wrong hotkey.
    pub dictation_threshold: f64,
    /// `is_destructive` at or above this forces spoken confirmation.
    pub destructive_threshold: f64,
}

/// Measured with `parlad --calibrate corpus/judge.toml` against
/// Qwen3-4B-Instruct-2507-Q4_K_M; the numbers and the method are in
/// docs/commands.md. The local model's option probabilities sit near 0 or
/// 1, so its floors are higher than TypeSafe's.
pub const LOCAL_THRESHOLDS: Thresholds = Thresholds {
    min_confidence: 0.35,
    act_unconfirmed_above: 0.65,
    dictation_threshold: 0.4,
    destructive_threshold: 0.8,
};

/// Hand-picked for TypeSafe's calibrated outputs; not re-measured.
pub const TYPESAFE_THRESHOLDS: Thresholds = Thresholds {
    min_confidence: 0.45,
    act_unconfirmed_above: 0.75,
    dictation_threshold: 0.5,
    destructive_threshold: 0.6,
};

impl Backend {
    /// The thresholds this backend runs with when the config names none.
    pub const fn default_thresholds(self) -> Thresholds {
        match self {
            Backend::Local => LOCAL_THRESHOLDS,
            Backend::TypeSafe => TYPESAFE_THRESHOLDS,
        }
    }
}

impl JudgeConfig {
    /// The thresholds in force: each key the config sets, and the backend's
    /// default for each it leaves out.
    pub fn thresholds(&self) -> Thresholds {
        let d = self.backend.default_thresholds();
        Thresholds {
            min_confidence: self.min_confidence.unwrap_or(d.min_confidence),
            act_unconfirmed_above: self
                .act_unconfirmed_above
                .unwrap_or(d.act_unconfirmed_above),
            dictation_threshold: self.dictation_threshold.unwrap_or(d.dictation_threshold),
            destructive_threshold: self
                .destructive_threshold
                .unwrap_or(d.destructive_threshold),
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Local => "local",
            Backend::TypeSafe => "typesafe",
        })
    }
}

/// The local model: llama.cpp with a GGUF on the GPU. Loaded once and
/// shared by the judged path and dictation cleanup when either names the
/// `local` backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    /// Path to a GGUF instruct model. A 3–4B model at Q4 fits next to
    /// whisper on an 8 GB card.
    pub model_path: PathBuf,
    /// Layers to offload to the GPU. More than the model has means all of
    /// them; 0 keeps the model on the CPU.
    pub gpu_layers: u32,
    /// Context size in tokens. One utterance's prompt is the desktop state
    /// plus one question, a few thousand tokens on a busy desktop.
    pub context_tokens: u32,
    /// CPU threads for whatever is not offloaded.
    pub threads: usize,
}

/// The TypeSafe backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TypeSafeConfig {
    /// API key. Prefer leaving this empty and exporting TYPESAFE_API_KEY, so
    /// the key stays out of a config file that gets copied around.
    pub api_key: Option<Secret>,
    pub model: String,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: Backend::Local,
            timeout_ms: 4_000,
            // None: the backend's own defaults apply, see `thresholds`.
            min_confidence: None,
            act_unconfirmed_above: None,
            dictation_threshold: None,
            destructive_threshold: None,
            send_window_titles: false,
            typesafe: TypeSafeConfig::default(),
        }
    }
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            model_path: dirs_model().join("Qwen3-4B-Instruct-2507-Q4_K_M.gguf"),
            gpu_layers: 999,
            context_tokens: 8_192,
            threads: 4,
        }
    }
}

impl Default for TypeSafeConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            model: "jev-latest".into(),
        }
    }
}

impl TypeSafeConfig {
    /// Environment wins over the config file.
    pub fn resolved_api_key(&self) -> anyhow::Result<String> {
        if let Ok(k) = std::env::var("TYPESAFE_API_KEY") {
            if !k.trim().is_empty() {
                return Ok(k);
            }
        }
        self.api_key
            .as_ref()
            .map(|k| k.expose().to_string())
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no TypeSafe API key (set TYPESAFE_API_KEY or judge.typesafe.api_key)"
                )
            })
    }
}

/// Dictation: what happens to a transcript before it is typed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlowConfig {
    /// Run the cleanup model over dictation: filler words out, self-
    /// corrections applied, punctuation fixed, the app's tone applied.
    /// Off types what whisper heard, after dictionary replacements.
    pub cleanup: bool,
    /// Who rewrites: the model under `[local]`, or an OpenAI-compatible
    /// chat completions endpoint.
    pub backend: FlowBackend,
    /// Give up on cleanup and type the raw transcript after this long.
    pub timeout_ms: u64,
    /// Most tokens the model may produce for one dictation.
    pub max_tokens: u32,
    /// Keep a history of dictations and commands in
    /// `$XDG_DATA_HOME/parla/history.jsonl` for the UI.
    pub history: bool,
    /// How long after a dictation "scratch that" or "make that formal"
    /// still refer to it.
    pub edit_window_ms: u64,
    /// Read the text around the cursor over AT-SPI at capture start and
    /// tell the cleanup model what the dictation continues. Needs
    /// `desktopd.a11y`; does nothing for a password field or the code tone.
    pub context: bool,
    /// What happens when a word is changed in the field right after a
    /// dictation. `suggest` records the pair in `learned.toml` for the UI
    /// to offer; `auto` also moves a pair seen twice into the dictionary;
    /// `off` reads nothing back. Needs `desktopd.a11y`.
    pub learn: LearnMode,
    /// How long after a dictation the field is read again for corrections.
    pub learn_after_ms: u64,
    pub openai: OpenAiConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LearnMode {
    /// Record corrections; the UI offers them for the dictionary.
    Suggest,
    /// Record them, and put a pair seen twice into the dictionary.
    Auto,
    Off,
}

impl std::fmt::Display for LearnMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LearnMode::Suggest => "suggest",
            LearnMode::Auto => "auto",
            LearnMode::Off => "off",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlowBackend {
    Local,
    OpenAi,
}

impl std::fmt::Display for FlowBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FlowBackend::Local => "local",
            FlowBackend::OpenAi => "openai",
        })
    }
}

/// Any server speaking the OpenAI chat completions API: OpenAI itself,
/// OpenRouter, Groq, or a local llama-server or Ollama.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiConfig {
    /// Base URL up to and excluding `/chat/completions`.
    pub base_url: String,
    /// Prefer exporting OPENAI_API_KEY instead.
    pub api_key: Option<Secret>,
    pub model: String,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            cleanup: true,
            backend: FlowBackend::Local,
            timeout_ms: 6_000,
            max_tokens: 1_024,
            history: true,
            edit_window_ms: 90_000,
            context: true,
            learn: LearnMode::Suggest,
            learn_after_ms: 20_000,
            openai: OpenAiConfig::default(),
        }
    }
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            api_key: None,
            model: "gpt-4.1-mini".into(),
        }
    }
}

impl OpenAiConfig {
    /// Environment wins over the config file. A local server may need no
    /// key at all, so an absent key is an empty string, not an error.
    pub fn resolved_api_key(&self) -> String {
        if let Ok(k) = std::env::var("OPENAI_API_KEY") {
            if !k.trim().is_empty() {
                return k;
            }
        }
        self.api_key
            .as_ref()
            .map(|k| k.expose().to_string())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    /// Chord held to dictate (transcript -> keystrokes into focused window).
    pub dictate: String,
    /// Chord held to issue a command (transcript -> fast-path router).
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Input device name (cpal); None = system default. Capture runs at the
    /// device's own rate and is resampled to the 16 kHz whisper expects.
    pub device: Option<String>,
    /// Silero VAD: a frame whose speech probability is at or above this
    /// counts as speech. Used when `asr.vad_model_path` exists.
    pub vad_threshold: f32,
    /// RMS energy gate, the fallback without a VAD model: frames above this
    /// count as speech.
    pub speech_threshold: f32,
    /// Hard cap on utterance length after silence trimming.
    pub max_utterance_ms: u64,
    /// Utterances shorter than this are dropped (accidental taps).
    pub min_utterance_ms: u64,
    /// Holding the hotkey longer than this finishes the capture as if the
    /// key had been released, so a lost release event cannot record forever.
    pub max_hold_ms: u64,
    /// Removed: capture always resamples to 16 kHz. Accepted so an older
    /// config still loads; `validate` warns when it is set.
    #[serde(skip_serializing)]
    pub sample_rate: Option<u32>,
    /// Removed: the energy gate has no end-of-speech timeout. Accepted so an
    /// older config still loads; `validate` warns when it is set.
    #[serde(skip_serializing)]
    pub end_silence_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    /// Path to the ggml model file.
    pub model_path: PathBuf,
    /// Path to whisper.cpp's Silero VAD model. When the file is absent the
    /// daemon starts anyway with the RMS energy gate in its place.
    pub vad_model_path: PathBuf,
    /// Whisper language code, or "auto" to detect.
    pub language: String,
    pub threads: usize,
    /// Bias the model toward domain vocabulary (plan P5: per-app hints).
    pub initial_prompt: Option<String>,
    /// Transcripts matching these (normalized, substring) are dropped as
    /// whisper silence-hallucinations.
    pub hallucination_blocklist: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterConfig {
    /// Extra grammar rules TOML file (see parla-grammar).
    pub grammar_file: Option<PathBuf>,
    /// Play audio cues on record start/stop.
    pub cues: bool,
    /// Show a notification with the action result.
    pub notify_results: bool,
    /// How long a "say yes" prompt stays answerable. A confirmation after
    /// this is refused and the action has to be spoken again.
    pub confirm_window_ms: u64,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            dictate: "ctrl+space".into(),
            command: "ctrl+shift+space".into(),
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: None,
            vad_threshold: 0.5,
            speech_threshold: 0.01,
            max_utterance_ms: 30_000,
            min_utterance_ms: 250,
            max_hold_ms: 30_000,
            sample_rate: None,
            end_silence_ms: None,
        }
    }
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            model_path: dirs_model().join("ggml-large-v3-turbo.bin"),
            vad_model_path: dirs_model().join("ggml-silero-v5.1.2.bin"),
            language: "en".into(),
            threads: 4,
            initial_prompt: None,
            hallucination_blocklist: vec![
                "thank you".into(),
                "thanks for watching".into(),
                "the end".into(),
                "subtitle".into(),
            ],
        }
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            grammar_file: None,
            cues: true,
            notify_results: true,
            confirm_window_ms: 8_000,
        }
    }
}

fn dirs_model() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".local/share")
        });
    base.join("parla/models")
}

impl DaemonConfig {
    /// Load from ~/.config/parla/parla.toml, falling back to defaults.
    pub fn load() -> anyhow::Result<Self> {
        let path = Self::path();
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let cfg: Self = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("bad config {}: {e}", path.display()))?;
            tracing::info!("loaded config from {}", path.display());
            cfg.validate()
                .map_err(|e| anyhow::anyhow!("bad config {}: {e}", path.display()))?;
            Ok(cfg)
        } else {
            tracing::info!("no config at {}; using defaults", path.display());
            let cfg = Self::default();
            cfg.validate()?;
            Ok(cfg)
        }
    }

    /// Reject values that would only fail later, at capture or judge time.
    pub fn validate(&self) -> anyhow::Result<()> {
        let a = &self.audio;
        if a.sample_rate.is_some() {
            tracing::warn!("audio.sample_rate is ignored: capture is always resampled to 16 kHz");
        }
        if a.end_silence_ms.is_some() {
            tracing::warn!("audio.end_silence_ms is ignored: the energy gate has no end-of-speech timeout");
        }
        check_unit("audio.speech_threshold", f64::from(a.speech_threshold))?;
        check_unit("audio.vad_threshold", f64::from(a.vad_threshold))?;
        anyhow::ensure!(a.max_utterance_ms > 0, "audio.max_utterance_ms must be > 0");
        anyhow::ensure!(a.max_hold_ms > 0, "audio.max_hold_ms must be > 0");
        anyhow::ensure!(
            a.min_utterance_ms <= a.max_utterance_ms,
            "audio.min_utterance_ms ({}) exceeds audio.max_utterance_ms ({})",
            a.min_utterance_ms,
            a.max_utterance_ms
        );

        anyhow::ensure!(self.asr.threads > 0, "asr.threads must be > 0");
        anyhow::ensure!(
            self.router.confirm_window_ms > 0,
            "router.confirm_window_ms must be > 0"
        );
        anyhow::ensure!(
            !self.asr.language.trim().is_empty(),
            "asr.language must be a language code or \"auto\""
        );

        crate::hotkeys::parse_chord(&self.hotkeys.dictate)
            .map_err(|e| anyhow::anyhow!("hotkeys.dictate: {e}"))?;
        crate::hotkeys::parse_chord(&self.hotkeys.command)
            .map_err(|e| anyhow::anyhow!("hotkeys.command: {e}"))?;

        let j = &self.judge;
        anyhow::ensure!(j.timeout_ms > 0, "judge.timeout_ms must be > 0");
        for (name, v) in [
            ("judge.min_confidence", j.min_confidence),
            ("judge.act_unconfirmed_above", j.act_unconfirmed_above),
            ("judge.dictation_threshold", j.dictation_threshold),
            ("judge.destructive_threshold", j.destructive_threshold),
        ] {
            if let Some(v) = v {
                check_unit(name, v)?;
            }
        }
        let t = j.thresholds();
        anyhow::ensure!(
            t.min_confidence <= t.act_unconfirmed_above,
            "judge.min_confidence ({}) exceeds judge.act_unconfirmed_above ({})",
            t.min_confidence,
            t.act_unconfirmed_above
        );
        anyhow::ensure!(
            self.local.context_tokens >= 512,
            "local.context_tokens must be at least 512"
        );
        anyhow::ensure!(self.local.threads > 0, "local.threads must be > 0");

        let fl = &self.flow;
        anyhow::ensure!(fl.timeout_ms > 0, "flow.timeout_ms must be > 0");
        anyhow::ensure!(fl.max_tokens > 0, "flow.max_tokens must be > 0");
        anyhow::ensure!(fl.learn_after_ms > 0, "flow.learn_after_ms must be > 0");
        anyhow::ensure!(
            !fl.openai.base_url.trim().is_empty(),
            "flow.openai.base_url must not be empty"
        );
        Ok(())
    }

    pub fn path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                    .join(".config")
            });
        base.join("parla/parla.toml")
    }

    /// Emit the default config file (parlad --print-default-config). The
    /// judge thresholds are printed commented out, with the numbers for
    /// both backends alongside: a key that is written stays in force when
    /// `backend` changes, one that is not follows the backend.
    pub fn default_toml() -> anyhow::Result<String> {
        let text = toml::to_string_pretty(&Self::default())?;
        let (l, t) = (LOCAL_THRESHOLDS, TYPESAFE_THRESHOLDS);
        let thresholds = format!(
            "# Defaults depend on the backend: local {}/{}/{}/{}, typesafe {}/{}/{}/{}; \
set a key to override.\n\
# min_confidence = {}\n# act_unconfirmed_above = {}\n# dictation_threshold = {}\n\
# destructive_threshold = {}\nsend_window_titles = ",
            l.min_confidence,
            l.act_unconfirmed_above,
            l.dictation_threshold,
            l.destructive_threshold,
            t.min_confidence,
            t.act_unconfirmed_above,
            t.dictation_threshold,
            t.destructive_threshold,
            l.min_confidence,
            l.act_unconfirmed_above,
            l.dictation_threshold,
            l.destructive_threshold,
        );
        anyhow::ensure!(
            text.matches("send_window_titles = ").count() == 1,
            "default config has no single [judge] send_window_titles key"
        );
        Ok(text.replacen("send_window_titles = ", &thresholds, 1))
    }
}

/// A probability or normalized level: finite and within [0, 1].
fn check_unit(name: &str, v: f64) -> anyhow::Result<()> {
    anyhow::ensure!(
        v.is_finite() && (0.0..=1.0).contains(&v),
        "{name} must be within [0, 1], got {v}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn older_audio_fields_still_load() {
        let cfg: DaemonConfig =
            toml::from_str("[audio]\nsample_rate = 48000\nend_silence_ms = 700\n").unwrap();
        assert_eq!(cfg.audio.sample_rate, Some(48000));
        cfg.validate().unwrap();
    }
    use super::*;

    #[test]
    fn defaults_validate() {
        DaemonConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_bad_thresholds() {
        let mut c = DaemonConfig::default();
        c.audio.speech_threshold = f32::NAN;
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.audio.speech_threshold = 1.5;
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.audio.vad_threshold = -0.1;
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.judge.min_confidence = Some(0.9);
        c.judge.act_unconfirmed_above = Some(0.5);
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.judge.dictation_threshold = Some(1.5);
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.local.context_tokens = 16;
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.audio.min_utterance_ms = 40_000;
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.audio.max_hold_ms = 0;
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.router.confirm_window_ms = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn thresholds_resolve_per_backend_and_print_in_full() {
        // Nothing set: each backend's own table.
        let cfg: DaemonConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.judge.thresholds(), LOCAL_THRESHOLDS);
        let cfg: DaemonConfig = toml::from_str("[judge]\nbackend = \"typesafe\"\n").unwrap();
        assert_eq!(cfg.judge.thresholds(), TYPESAFE_THRESHOLDS);
        assert_eq!(cfg.judge.thresholds().act_unconfirmed_above, 0.75);

        // A key that is set wins over the table, for that key only.
        let cfg: DaemonConfig =
            toml::from_str("[judge]\nbackend = \"typesafe\"\nmin_confidence = 0.3\n").unwrap();
        let t = cfg.judge.thresholds();
        assert_eq!(t.min_confidence, 0.3);
        assert_eq!(t.dictation_threshold, TYPESAFE_THRESHOLDS.dictation_threshold);

        // The printed defaults show all four keys commented out, with the
        // local numbers, under [judge] and before send_window_titles, and
        // load back with no key set, so the backend's table applies.
        let text = DaemonConfig::default_toml().unwrap();
        let judge = text.find("[judge]").unwrap();
        for key in [
            "\n# min_confidence = 0.35\n",
            "\n# act_unconfirmed_above = 0.65\n",
            "\n# dictation_threshold = 0.4\n",
            "\n# destructive_threshold = 0.8\n",
            "typesafe 0.45/0.75/0.5/0.6",
        ] {
            let at = text.find(key).unwrap_or_else(|| panic!("{key:?} missing from\n{text}"));
            assert!(at > judge && at < text.find("send_window_titles").unwrap(), "{text}");
        }
        assert!(!text.contains("\nmin_confidence = "), "{text}");
        let back: DaemonConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.judge.thresholds(), LOCAL_THRESHOLDS);
        assert_eq!(back.judge.min_confidence, None);
        back.validate().unwrap();
    }

    #[test]
    fn secret_never_prints() {
        let c = TypeSafeConfig {
            api_key: Some(Secret("sk-live-1234".into())),
            ..Default::default()
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("sk-live"), "{dbg}");
        let toml = toml::to_string(&c).unwrap();
        assert!(!toml.contains("sk-live"), "{toml}");
        let parsed: TypeSafeConfig = toml::from_str("api_key = \"abc\"").unwrap();
        assert_eq!(parsed.api_key.as_ref().unwrap().expose(), "abc");
    }

    #[test]
    fn default_toml_round_trips_with_vad_keys() {
        let text = DaemonConfig::default_toml().unwrap();
        assert!(text.contains("vad_model_path = "), "{text}");
        assert!(text.contains("vad_threshold = 0.5"), "{text}");
        let cfg: DaemonConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.asr.vad_model_path, AsrConfig::default().vad_model_path);
        assert_eq!(
            cfg.asr.vad_model_path.file_name().unwrap(),
            "ggml-silero-v5.1.2.bin"
        );
    }

    #[test]
    fn backend_is_spelled_in_lowercase() {
        let cfg: DaemonConfig = toml::from_str("[judge]\nbackend = \"typesafe\"\n").unwrap();
        assert_eq!(cfg.judge.backend, Backend::TypeSafe);
        let text = DaemonConfig::default_toml().unwrap();
        assert!(text.contains("backend = \"local\""), "{text}");
        assert!(text.contains("[local]"), "{text}");
        assert!(text.contains("[flow.openai]"), "{text}");
        let cfg: DaemonConfig = toml::from_str("[flow]\nbackend = \"openai\"\n").unwrap();
        assert_eq!(cfg.flow.backend, FlowBackend::OpenAi);
    }

    #[test]
    fn learn_mode_round_trips() {
        let text = DaemonConfig::default_toml().unwrap();
        assert!(text.contains("learn = \"suggest\""), "{text}");
        assert!(text.contains("learn_after_ms = 20000"), "{text}");
        let cfg: DaemonConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.flow.learn, LearnMode::Suggest);
        let cfg: DaemonConfig = toml::from_str("[flow]\nlearn = \"auto\"\n").unwrap();
        assert_eq!(cfg.flow.learn, LearnMode::Auto);
        let cfg: DaemonConfig = toml::from_str("[flow]\nlearn = \"off\"\n").unwrap();
        assert_eq!(cfg.flow.learn, LearnMode::Off);
        assert!(toml::from_str::<DaemonConfig>("[flow]\nlearn = \"always\"\n").is_err());
        let mut c = DaemonConfig::default();
        c.flow.learn_after_ms = 0;
        assert!(c.validate().is_err());
    }
}
