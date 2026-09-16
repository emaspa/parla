use std::collections::BTreeMap;

use serde::Deserialize;

use crate::intent::Intent;

/// A grammar rule as it appears in config: a pattern with at most one
/// trailing `{slot}` capture, an intent name, and optional fixed args.
///
/// ```toml
/// [[grammar.rule]]
/// pattern = "take a screenshot"
/// intent = "run_shortcut"
/// args = { component = "org_kde_spectacle_desktop", action = "ActiveWindowScreenShot" }
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct RuleDef {
    pub pattern: String,
    pub intent: String,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    /// Max words the trailing capture may swallow. Keeps long prose
    /// ("open my email and find the message from alan...") off the fast path
    /// so it falls through to the agent instead of becoming a bogus app name.
    #[serde(default)]
    pub max_capture_words: Option<usize>,
}

/// Compiled form: literal prefix words plus an optional trailing capture slot.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub prefix: Vec<String>,
    /// Name of the trailing capture slot, if the pattern ends with `{name}`.
    pub slot: Option<String>,
    pub intent: String,
    pub args: BTreeMap<String, String>,
    pub max_capture_words: Option<usize>,
}

impl CompiledRule {
    pub fn compile(def: &RuleDef) -> Option<Self> {
        let pattern = def.pattern.trim().to_lowercase();
        let mut prefix = Vec::new();
        let mut slot = None;
        for token in pattern.split_whitespace() {
            if let Some(inner) = token
                .strip_prefix('{')
                .and_then(|t| t.strip_suffix('}'))
            {
                // captures only allowed as the final token
                slot = Some(inner.to_string());
            } else if slot.is_some() {
                tracing::warn!("ignoring token after capture in pattern {:?}", def.pattern);
                return None;
            } else {
                prefix.push(token.to_string());
            }
        }
        if prefix.is_empty() && slot.is_none() {
            return None;
        }
        Some(Self {
            prefix,
            slot,
            intent: def.intent.clone(),
            args: def.args.clone(),
            max_capture_words: def.max_capture_words,
        })
    }

    /// Match a normalized utterance. Returns the captured trailing words, if
    /// the rule matches.
    pub fn match_utterance<'a>(&self, words: &'a [&'a str]) -> Option<Option<&'a [&'a str]>> {
        if words.len() < self.prefix.len() {
            return None;
        }
        for (expected, got) in self.prefix.iter().zip(words) {
            if expected != got {
                return None;
            }
        }
        let rest = &words[self.prefix.len()..];
        match &self.slot {
            None => {
                if rest.is_empty() {
                    Some(None)
                } else {
                    None
                }
            }
            Some(_) => {
                if rest.is_empty() {
                    None // capture slot must actually capture something
                } else if self.max_capture_words.is_some_and(|max| rest.len() > max) {
                    None // prose, not a command — let it fall through to the agent
                } else {
                    Some(Some(rest))
                }
            }
        }
    }

    /// Build the intent, merging the captured slot into args under its name.
    pub fn build_intent(&self, capture: Option<&[&str]>) -> Option<Intent> {
        let mut args = self.args.clone();
        if let (Some(slot), Some(cap)) = (&self.slot, capture) {
            args.insert(slot.clone(), capture_string(cap));
        }
        Intent::from_args(&self.intent, &args)
    }
}

/// Rest-capture slot value joined from the trailing words.
pub(crate) fn capture_string(words: &[&str]) -> String {
    words.join(" ")
}
