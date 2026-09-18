//! Per-application profiles: how dictated text should read where it lands.
//! A chat window wants a relaxed register, a mail client a proper one, a
//! terminal wants what was said turned into symbols and nothing invented.
//!
//! ```toml
//! [[app]]
//! name = "Terminals"
//! class = ["konsole", "kitty"]
//! tone = "code"
//! instructions = "Commands and paths, nothing else."
//! ```
//!
//! `class` entries match the window class case-insensitively as substrings,
//! so "konsole" covers "org.kde.konsole". The first profile that matches
//! wins. A window no profile matches gets [`AppProfile::fallback`].

use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    /// Clean punctuation and capitalisation, the speaker's own register.
    #[default]
    Neutral,
    /// Messages: relaxed, contractions kept, no sign-offs invented.
    Casual,
    /// Mail and documents: complete sentences, paragraphs.
    Formal,
    /// Terminals and editors: spoken symbols become symbols, identifiers
    /// stay literal, no trailing period.
    Code,
}

impl Tone {
    /// What the cleanup model is told about the register.
    pub fn guidance(self) -> &'static str {
        match self {
            Tone::Neutral => "Keep the speaker's register. Use standard punctuation and capitalisation.",
            Tone::Casual => "This is a chat message. Keep it relaxed and short, keep contractions, do not add greetings or sign-offs.",
            Tone::Formal => "This is for an email or a document. Use complete sentences, proper capitalisation and paragraph breaks where the speaker moved to a new point.",
            Tone::Code => "This goes into a terminal or code editor. Turn spoken symbols into symbols (\"dash\" -> -, \"underscore\" -> _, \"slash\" -> /, \"dot\" -> .), keep identifiers, paths and commands literal, and do not add a period at the end.",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tone::Neutral => "neutral",
            Tone::Casual => "casual",
            Tone::Formal => "formal",
            Tone::Code => "code",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppProfile {
    pub name: String,
    /// Window class substrings, case-insensitive.
    pub class: Vec<String>,
    pub tone: Tone,
    /// Run the cleanup model at all. Off types the transcript as heard,
    /// after dictionary replacements.
    pub cleanup: bool,
    /// Free text appended to the cleanup instructions for this app.
    pub instructions: String,
}

impl Default for AppProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            class: Vec::new(),
            tone: Tone::Neutral,
            cleanup: true,
            instructions: String::new(),
        }
    }
}

impl AppProfile {
    /// The profile for windows nothing else matches.
    pub fn fallback() -> Self {
        Self {
            name: "Everything else".into(),
            ..Self::default()
        }
    }

    pub fn matches(&self, class: &str) -> bool {
        let class = class.to_lowercase();
        self.class
            .iter()
            .any(|c| !c.is_empty() && class.contains(&c.to_lowercase()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppProfiles {
    #[serde(rename = "app")]
    pub apps: Vec<AppProfile>,
}

impl Default for AppProfiles {
    /// The profiles a fresh install starts with. Written to disk on first
    /// save, so the UI shows something to edit.
    fn default() -> Self {
        let profile = |name: &str, class: &[&str], tone, instructions: &str| AppProfile {
            name: name.into(),
            class: class.iter().map(|c| c.to_string()).collect(),
            tone,
            cleanup: true,
            instructions: instructions.into(),
        };
        Self {
            apps: vec![
                profile(
                    "Terminals",
                    &[
                        "konsole",
                        "kitty",
                        "alacritty",
                        "foot",
                        "wezterm",
                        "ghostty",
                        "yakuake",
                    ],
                    Tone::Code,
                    "",
                ),
                profile(
                    "Code editors",
                    &[
                        "code",
                        "codium",
                        "kate",
                        "neovide",
                        "zed",
                        "jetbrains",
                        "idea",
                        "cursor",
                    ],
                    Tone::Code,
                    "",
                ),
                profile(
                    "Chat",
                    &[
                        "slack",
                        "discord",
                        "telegram",
                        "signal",
                        "element",
                        "whatsapp",
                        "neochat",
                        "konversation",
                    ],
                    Tone::Casual,
                    "",
                ),
                profile(
                    "Mail and documents",
                    &[
                        "thunderbird",
                        "kmail",
                        "evolution",
                        "libreoffice",
                        "onlyoffice",
                        "kontact",
                    ],
                    Tone::Formal,
                    "",
                ),
            ],
        }
    }
}

impl AppProfiles {
    /// Read the file, or the built-in defaults when it does not exist yet.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(toml_str: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(toml_str)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        crate::paths::write_atomic(path, &toml::to_string_pretty(self)?)
    }

    /// The profile for a window class: first match wins, else the fallback.
    pub fn for_class(&self, class: &str) -> AppProfile {
        self.apps
            .iter()
            .find(|p| p.matches(class))
            .cloned()
            .unwrap_or_else(AppProfile::fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_common_classes() {
        let p = AppProfiles::default();
        assert_eq!(p.for_class("org.kde.konsole").tone, Tone::Code);
        assert_eq!(p.for_class("Slack").tone, Tone::Casual);
        assert_eq!(p.for_class("thunderbird").tone, Tone::Formal);
        assert_eq!(p.for_class("firefox").name, "Everything else");
        assert_eq!(p.for_class("firefox").tone, Tone::Neutral);
    }

    #[test]
    fn file_replaces_defaults_entirely() {
        let p = AppProfiles::parse(
            r#"
            [[app]]
            name = "Raw"
            class = ["konsole"]
            cleanup = false
            "#,
        )
        .unwrap();
        assert_eq!(p.apps.len(), 1);
        assert!(!p.for_class("konsole").cleanup);
        assert_eq!(p.for_class("konsole").tone, Tone::Neutral);
        assert!(p.for_class("slack").cleanup);
    }

    #[test]
    fn tone_is_spelled_in_lowercase() {
        let p = AppProfiles::parse("[[app]]\nname='x'\ntone='formal'").unwrap();
        assert_eq!(p.apps[0].tone, Tone::Formal);
        assert!(AppProfiles::parse("[[app]]\nname='x'\ntone='Formal'").is_err());
        let s = toml::to_string(&AppProfiles::default()).unwrap();
        assert!(s.contains("tone = \"code\""));
    }
}
