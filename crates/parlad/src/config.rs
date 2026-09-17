use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub hotkeys: HotkeyConfig,
    pub audio: AudioConfig,
    pub asr: AsrConfig,
    pub router: RouterConfig,
    pub typesafe: TypeSafeConfig,
    pub desktopd: desktopd::DesktopdConfig,
}

/// The judged path: what happens to utterances the grammar rejects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TypeSafeConfig {
    /// Judge utterances the fast-path grammar does not match.
    pub enabled: bool,
    /// API key. Prefer leaving this empty and exporting TYPESAFE_API_KEY, so
    /// the key stays out of a config file that gets copied around.
    pub api_key: Option<String>,
    pub model: String,
    /// Give up rather than keep the user waiting on a voice command.
    pub timeout_ms: u64,
    /// Below this intent confidence, act on nothing.
    pub min_confidence: f64,
    /// At or above this, act without a confirmation prompt — unless the action
    /// is judged destructive, which always prompts.
    pub act_unconfirmed_above: f64,
    /// `is_dictation` at or above this means the user held the wrong hotkey.
    pub dictation_threshold: f64,
    /// `is_destructive` at or above this forces spoken confirmation.
    pub destructive_threshold: f64,
}

impl Default for TypeSafeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            model: "jev-latest".into(),
            timeout_ms: 2_500,
            // Starting points only: these want tuning against real
            // utterances, plotting confidence against whether the action
            // was the one wanted.
            min_confidence: 0.45,
            act_unconfirmed_above: 0.75,
            dictation_threshold: 0.5,
            destructive_threshold: 0.6,
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
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("no TypeSafe API key (set TYPESAFE_API_KEY or typesafe.api_key)")
            })
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
    /// Input device name (cpal); None = system default.
    pub device: Option<String>,
    /// Model/sample rate whisper expects.
    pub sample_rate: u32,
    /// RMS energy gate: frames above this count as speech.
    pub speech_threshold: f32,
    /// Trailing silence (ms) that ends an utterance.
    pub end_silence_ms: u64,
    /// Hard cap on utterance length.
    pub max_utterance_ms: u64,
    /// Utterances shorter than this are dropped (accidental taps).
    pub min_utterance_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    /// Path to the ggml model file.
    pub model_path: PathBuf,
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
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            hotkeys: HotkeyConfig::default(),
            audio: AudioConfig::default(),
            asr: AsrConfig::default(),
            router: RouterConfig::default(),
            typesafe: TypeSafeConfig::default(),
            desktopd: desktopd::DesktopdConfig::default(),
        }
    }
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
            sample_rate: 16_000,
            speech_threshold: 0.01,
            end_silence_ms: 700,
            max_utterance_ms: 30_000,
            min_utterance_ms: 250,
        }
    }
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            model_path: dirs_model().join("ggml-large-v3-turbo.bin"),
            language: "en".into(),
            threads: 4,
            initial_prompt: None,
            hallucination_blocklist: vec![
                "thank you".into(),
                "thanks for watching".into(),
                "you".into(),
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
            Ok(cfg)
        } else {
            tracing::info!("no config at {}; using defaults", path.display());
            Ok(Self::default())
        }
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

    /// Emit the default config file (parlad --print-default-config).
    pub fn default_toml() -> anyhow::Result<String> {
        Ok(toml::to_string_pretty(&Self::default())?)
    }
}
