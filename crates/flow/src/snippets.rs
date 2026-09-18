//! Snippets: text inserted by saying its name. "insert my email" types the
//! address; so does just "my email" on its own.
//!
//! ```toml
//! [[snippet]]
//! trigger = "my email"
//! text = "someone@example.com"
//! ```

use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::text::normalize;

/// Words that may precede a trigger. "insert my email", "paste my email".
const INSERT_VERBS: &[&str] = &["insert", "paste", "type", "put in"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Snippets {
    #[serde(rename = "snippet")]
    pub snippets: Vec<Snippet>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snippet {
    /// Spoken name; matched after normalisation, so case and punctuation do
    /// not matter.
    pub trigger: String,
    pub text: String,
}

impl Snippets {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(toml_str: &str) -> anyhow::Result<Self> {
        let mut s: Self = toml::from_str(toml_str)?;
        s.tidy();
        Ok(s)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut s = self.clone();
        s.tidy();
        crate::paths::write_atomic(path, &toml::to_string_pretty(&s)?)
    }

    fn tidy(&mut self) {
        self.snippets.retain_mut(|s| {
            s.trigger = s.trigger.trim().to_string();
            !normalize(&s.trigger).is_empty()
        });
    }

    /// The snippet an utterance asks for: the whole utterance is a trigger,
    /// optionally preceded by an insert verb. Anything else returns None,
    /// so ordinary prose that happens to contain a trigger is left alone.
    pub fn expand(&self, utterance: &str) -> Option<&Snippet> {
        let said = normalize(utterance);
        if said.is_empty() {
            return None;
        }
        self.snippets.iter().find(|s| {
            let trigger = normalize(&s.trigger);
            said == trigger
                || INSERT_VERBS
                    .iter()
                    .any(|v| said == format!("{v} {trigger}"))
        })
    }

    pub fn is_empty(&self) -> bool {
        self.snippets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippets() -> Snippets {
        Snippets::parse(
            r#"
            [[snippet]]
            trigger = "My Email"
            text = "me@example.com"
            [[snippet]]
            trigger = "  "
            text = "never"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn matches_bare_and_verbed_triggers() {
        let s = snippets();
        assert_eq!(s.snippets.len(), 1, "blank trigger dropped");
        assert_eq!(s.expand("my email").unwrap().text, "me@example.com");
        assert_eq!(s.expand("Insert my e-mail.").map(|s| s.text.as_str()), None);
        assert_eq!(s.expand("insert my email").unwrap().text, "me@example.com");
        assert_eq!(s.expand("paste my email").unwrap().text, "me@example.com");
        assert!(s.expand("send it to my email please").is_none());
        assert!(s.expand("").is_none());
    }
}
