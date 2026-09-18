//! What the judged path asks and what it gets back, independent of who
//! answers.
//!
//! One evaluation carries a JSON `state` plus a set of questions about it.
//! Each question is answered on its own: a yes/no question yields the
//! probability of yes, a choice question yields one option key with its
//! probability. The judge builds the questions and reads the answers; which
//! model answers them, and where it runs, is the [`Oracle`]'s business.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

use crate::local::LocalModel;
use crate::typesafe::Client;

/// The largest option set a choice question may carry. TypeSafe refuses more
/// than this; the local backend scores every option, so the same cap bounds
/// its work too.
pub const MAX_CHOICE_OPTIONS: usize = 255;

/// A question's `instructions` or a criteria entry: a bare string, or a
/// structured object whose field names we choose (the model sees both names
/// and values).
pub type Prose = serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// Yes/no. The answer is the probability of yes — there is no separate
    /// confidence field.
    Noul {
        instructions: Prose,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// One option from a defined set (at most [`MAX_CHOICE_OPTIONS`]). Always
    /// give it a no-match option when the set may not cover the input.
    Choice {
        instructions: Prose,
        criteria: Criteria,
    },
    /// Position along ordered levels (2–10). Unused so far, kept because the
    /// wire type is part of the TypeSafe API surface.
    #[allow(dead_code)]
    Score {
        instructions: Prose,
        criteria: Vec<Prose>,
    },
}

/// The options of a choice question, in the order the model reads them.
/// A JSON object on the wire, but ordered: the local model sees the list
/// as rendered, and a "none of these" option belongs at its end, after
/// the candidates it rejects.
#[derive(Debug, Clone, Default)]
pub struct Criteria(Vec<(String, Prose)>);

impl Criteria {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an option; a key already present is replaced in place.
    pub fn insert(&mut self, key: impl Into<String>, desc: Prose) {
        let key = key.into();
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = desc,
            None => self.0.push((key, desc)),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Prose> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Prose)> {
        self.0.iter().map(|(k, v)| (k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(k, _)| k)
    }
}

impl<K: Into<String>, const N: usize> From<[(K, Prose); N]> for Criteria {
    fn from(items: [(K, Prose); N]) -> Self {
        let mut c = Self::new();
        for (k, v) in items {
            c.insert(k, v);
        }
        c
    }
}

impl<K: Into<String>> FromIterator<(K, Prose)> for Criteria {
    fn from_iter<I: IntoIterator<Item = (K, Prose)>>(iter: I) -> Self {
        let mut c = Self::new();
        for (k, v) in iter {
            c.insert(k, v);
        }
        c
    }
}

impl Serialize for Criteria {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl Question {
    /// The option names a choice question offers; None for other kinds.
    pub fn options(&self) -> Option<&Criteria> {
        match self {
            Question::Choice { criteria, .. } => Some(criteria),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NoulCriteria {
    /// What a value near 1 means.
    #[serde(rename = "true")]
    pub yes: String,
    /// What a value near 0 means.
    #[serde(rename = "false")]
    pub no: String,
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub answers: BTreeMap<String, Answer>,
    /// Token accounting. Optional: it is informational, and its absence must
    /// not throw away the answers.
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        #[allow(dead_code)]
        probabilities: BTreeMap<String, f64>,
    },
    #[allow(dead_code)]
    Score {
        score: f64,
        confidence: f64,
    },
    /// An answer type this client does not know. Reads as "not answered", so
    /// a new type on the API side degrades one question, not the request.
    #[serde(other)]
    Unknown,
}

impl Response {
    /// Probability of yes for a noul question, or None if absent/wrong type.
    pub fn noul(&self, id: &str) -> Option<f64> {
        match self.answers.get(id) {
            Some(Answer::Noul { noul }) => Some(*noul),
            _ => None,
        }
    }

    /// Chosen option and its confidence, or None if absent/wrong type.
    pub fn choice(&self, id: &str) -> Option<(&str, f64)> {
        match self.answers.get(id) {
            Some(Answer::Choice {
                choice, confidence, ..
            }) => Some((choice.as_str(), *confidence)),
            _ => None,
        }
    }
}

/// Who answers the questions.
pub enum Oracle {
    /// The GGUF model on this machine's GPU, shared with dictation cleanup.
    /// Nothing leaves the host.
    Local(Arc<LocalModel>, Duration),
    /// The TypeSafe System One API over HTTPS.
    TypeSafe(Client),
}

impl Oracle {
    pub async fn evaluate(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> anyhow::Result<Response> {
        match self {
            Oracle::Local(m, timeout) => m.evaluate(state, questions, *timeout).await,
            Oracle::TypeSafe(c) => c.evaluate(state, questions).await,
        }
    }

    /// Does an evaluation stay on this machine?
    pub fn is_local(&self) -> bool {
        matches!(self, Oracle::Local(..))
    }

    /// What to call the model in a log line.
    pub fn describe(&self) -> String {
        match self {
            Oracle::Local(m, _) => format!("local {}", m.name()),
            Oracle::TypeSafe(c) => format!("typesafe {}", c.model()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: serde_json::Value) -> Response {
        serde_json::from_value(v).expect("response should deserialize")
    }

    #[test]
    fn criteria_keep_insertion_order_and_serialize_as_an_object() {
        let mut c = Criteria::new();
        c.insert("b", serde_json::json!("second"));
        c.insert("a", serde_json::json!("first"));
        c.insert("b", serde_json::json!("replaced"));
        assert_eq!(c.keys().cloned().collect::<Vec<_>>(), vec!["b", "a"]);
        assert_eq!(c.len(), 2);
        assert!(c.contains_key("a") && !c.contains_key("c"));
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"b":"replaced","a":"first"}"#
        );
    }

    #[test]
    fn missing_usage_keeps_the_answers() {
        let r = parse(serde_json::json!({
            "model": "jev-latest",
            "answers": { "is_dictation": { "type": "noul", "noul": 0.1 } },
        }));
        assert!(r.usage.is_none());
        assert_eq!(r.noul("is_dictation"), Some(0.1));
    }

    #[test]
    fn unknown_answer_type_degrades_one_question_only() {
        let r = parse(serde_json::json!({
            "answers": {
                "intent": { "type": "ranking", "ranking": ["a", "b"] },
                "is_dictation": { "type": "noul", "noul": 0.2 },
            },
            "usage": { "input_tokens": 1, "output_tokens": 2 },
        }));
        assert!(matches!(r.answers.get("intent"), Some(Answer::Unknown)));
        assert_eq!(r.choice("intent"), None);
        assert_eq!(r.noul("is_dictation"), Some(0.2));
    }

    #[test]
    fn wrong_answer_type_for_an_id_reads_as_unanswered() {
        let r = parse(serde_json::json!({
            "answers": {
                "is_dictation": {
                    "type": "choice", "choice": "yes", "confidence": 0.9,
                    "probabilities": { "yes": 0.9 },
                },
                "intent": { "type": "noul", "noul": 0.9 },
            },
        }));
        assert_eq!(r.noul("is_dictation"), None);
        assert_eq!(r.choice("intent"), None);
    }
}
