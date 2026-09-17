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
    pub api_key: Option<Secret>,
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
    /// Include window titles in what the judged path sends to the API. Off by
    /// default: titles carry document names, URLs and chat subjects, so the
    /// request then names only each window's application and an index, and
    /// the title is mapped back on this machine.
    pub send_window_titles: bool,
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
            send_window_titles: false,
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
    /// Input device name (cpal); None = system default. Capture runs at the
    /// device's own rate and is resampled to the 16 kHz whisper expects.
    pub device: Option<String>,
    /// RMS energy gate: frames above this count as speech.
    pub speech_threshold: f32,
    /// Hard cap on utterance length after silence trimming.
    pub max_utterance_ms: u64,
    /// Utterances shorter than this are dropped (accidental taps).
    pub min_utterance_ms: u64,
    /// Holding the hotkey longer than this finishes the capture as if the
    /// key had been released, so a lost release event cannot record forever.
    pub max_hold_ms: u64,
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
            speech_threshold: 0.01,
            max_utterance_ms: 30_000,
            min_utterance_ms: 250,
            max_hold_ms: 30_000,
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
        check_unit("audio.speech_threshold", f64::from(a.speech_threshold))?;
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
            !self.asr.language.trim().is_empty(),
            "asr.language must be a language code or \"auto\""
        );

        crate::hotkeys::parse_chord(&self.hotkeys.dictate)
            .map_err(|e| anyhow::anyhow!("hotkeys.dictate: {e}"))?;
        crate::hotkeys::parse_chord(&self.hotkeys.command)
            .map_err(|e| anyhow::anyhow!("hotkeys.command: {e}"))?;

        let t = &self.typesafe;
        anyhow::ensure!(t.timeout_ms > 0, "typesafe.timeout_ms must be > 0");
        for (name, v) in [
            ("typesafe.min_confidence", t.min_confidence),
            ("typesafe.act_unconfirmed_above", t.act_unconfirmed_above),
            ("typesafe.dictation_threshold", t.dictation_threshold),
            ("typesafe.destructive_threshold", t.destructive_threshold),
        ] {
            check_unit(name, v)?;
        }
        anyhow::ensure!(
            t.min_confidence <= t.act_unconfirmed_above,
            "typesafe.min_confidence ({}) exceeds typesafe.act_unconfirmed_above ({})",
            t.min_confidence,
            t.act_unconfirmed_above
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

    /// Emit the default config file (parlad --print-default-config).
    pub fn default_toml() -> anyhow::Result<String> {
        Ok(toml::to_string_pretty(&Self::default())?)
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
        c.typesafe.min_confidence = 0.9;
        c.typesafe.act_unconfirmed_above = 0.5;
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.audio.min_utterance_ms = 40_000;
        assert!(c.validate().is_err());
        let mut c = DaemonConfig::default();
        c.audio.max_hold_ms = 0;
        assert!(c.validate().is_err());
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
}
