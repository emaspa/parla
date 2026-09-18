//! The personal dictionary: words whisper should know how to spell, and
//! spoken-to-written replacements applied to every dictation.
//!
//! ```toml
//! words = ["Emanuele", "parla", "KWin", "llama.cpp"]
//!
//! [[replace]]
//! spoken = "e-mail"
//! written = "email"
//! ```

use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::text::replace_word;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Dictionary {
    /// Names, jargon and acronyms as they should be written. They bias the
    /// transcriber and the cleanup model is told to spell them exactly so.
    pub words: Vec<String>,
    /// Deterministic substitutions, applied whole-word and case-insensitive
    /// to the transcript before cleanup and to the text after it.
    #[serde(rename = "replace")]
    pub replacements: Vec<Replacement>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub spoken: String,
    pub written: String,
}

impl Dictionary {
    /// Read the file, or an empty dictionary when it does not exist yet.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(toml_str: &str) -> anyhow::Result<Self> {
        let mut d: Self = toml::from_str(toml_str)?;
        d.tidy();
        Ok(d)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut d = self.clone();
        d.tidy();
        crate::paths::write_atomic(path, &toml::to_string_pretty(&d)?)
    }

    /// Trim, drop empties and duplicates, keep first-seen order.
    fn tidy(&mut self) {
        let mut seen = std::collections::BTreeSet::new();
        self.words = self
            .words
            .iter()
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty() && seen.insert(w.to_lowercase()))
            .collect();
        self.replacements.retain_mut(|r| {
            r.spoken = r.spoken.trim().to_string();
            r.written = r.written.trim().to_string();
            !r.spoken.is_empty()
        });
    }

    /// Words as a whisper initial prompt: a comma-separated list biases the
    /// decoder toward these spellings without being mistaken for speech.
    pub fn asr_prompt(&self) -> Option<String> {
        if self.words.is_empty() {
            None
        } else {
            Some(self.words.join(", "))
        }
    }

    pub fn apply_replacements(&self, text: &str) -> String {
        self.replacements.iter().fold(text.to_string(), |t, r| {
            replace_word(&t, &r.spoken, &r.written)
        })
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.replacements.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_tidies() {
        let d = Dictionary::parse(
            r#"
            words = ["Emanuele", " parla ", "emanuele", ""]
            [[replace]]
            spoken = "e-mail"
            written = "email"
            [[replace]]
            spoken = ""
            written = "x"
            "#,
        )
        .unwrap();
        assert_eq!(d.words, vec!["Emanuele", "parla"]);
        assert_eq!(d.replacements.len(), 1);
        assert_eq!(d.asr_prompt().as_deref(), Some("Emanuele, parla"));
        assert_eq!(
            d.apply_replacements("My E-mail is here"),
            "My email is here"
        );
    }

    #[test]
    fn empty_file_is_empty_dictionary() {
        let d = Dictionary::parse("").unwrap();
        assert!(d.is_empty());
        assert_eq!(d.asr_prompt(), None);
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(Dictionary::parse("wrods = []").is_err());
    }

    #[test]
    fn round_trips_through_toml() {
        let d = Dictionary {
            words: vec!["KWin".into()],
            replacements: vec![Replacement {
                spoken: "kay win".into(),
                written: "KWin".into(),
            }],
        };
        let s = toml::to_string_pretty(&d).unwrap();
        assert_eq!(Dictionary::parse(&s).unwrap(), d);
    }
}
