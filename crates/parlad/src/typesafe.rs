//! Minimal TypeSafe System One client (`POST /v1/systemone`).
//!
//! There is no Rust SDK, so this is the HTTP API directly: one request carries
//! the state plus every question, the model answers them independently and in
//! parallel, and answers come back under the ids we chose. Retries transient
//! failures with backoff under one overall deadline, which is the one thing
//! the official SDKs add.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

/// The API refuses a choice question with more options than this.
pub const MAX_CHOICE_OPTIONS: usize = 255;

const MAX_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);

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
        criteria: BTreeMap<String, Prose>,
    },
    /// Position along ordered levels (2–10). Unused so far, kept because the
    /// wire type is part of the API surface.
    #[allow(dead_code)]
    Score {
        instructions: Prose,
        criteria: Vec<Prose>,
    },
}

impl Question {
    /// The option names a choice question offers; None for other kinds.
    pub fn options(&self) -> Option<&BTreeMap<String, Prose>> {
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

#[derive(Debug, Serialize)]
struct Request<'a> {
    state: &'a serde_json::Value,
    model: &'a str,
    questions: &'a BTreeMap<String, Question>,
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub answers: BTreeMap<String, Answer>,
    /// Token accounting. Optional: it is informational, and its absence must
    /// not throw away the answers.
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
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

pub struct Client {
    http: reqwest::Client,
    api_key: String,
    model: String,
    /// Wall-clock budget for one `evaluate`, retries and backoff included.
    timeout: Duration,
}

/// Transient: the same request may succeed a moment later.
fn retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 529)
}

/// `Retry-After` as a delay. Only the delay-seconds form is understood; the
/// HTTP-date form falls back to our own backoff.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

impl Client {
    pub fn new(api_key: String, model: String, timeout: Duration) -> anyhow::Result<Self> {
        anyhow::ensure!(!api_key.trim().is_empty(), "empty TypeSafe API key");
        Ok(Self {
            // Per-request cap; the overall deadline in `evaluate` is what
            // bounds the user's wait.
            http: reqwest::Client::builder().timeout(timeout).build()?,
            api_key,
            model,
            timeout,
        })
    }

    /// Evaluate every question against one state within `timeout` of wall
    /// time, whatever the retries do. Transient statuses (429, 5xx, 529) are
    /// retried with exponential backoff, honoring `Retry-After` when it fits
    /// the deadline; 401/422 fail immediately since retrying a malformed
    /// request or a bad key never helps.
    pub async fn evaluate(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> anyhow::Result<Response> {
        let body = Request {
            state,
            model: &self.model,
            questions,
        };
        let deadline = Instant::now() + self.timeout;
        match tokio::time::timeout_at(deadline, self.attempts(&body, deadline)).await {
            Ok(result) => result,
            Err(_) => anyhow::bail!(
                "typesafe: no answer within {} ms",
                self.timeout.as_millis()
            ),
        }
    }

    async fn attempts(&self, body: &Request<'_>, deadline: Instant) -> anyhow::Result<Response> {
        let mut backoff = INITIAL_BACKOFF;
        let mut last_err = None;

        for attempt in 1..=MAX_ATTEMPTS {
            let resp = self
                .http
                .post(ENDPOINT)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await;
            let wait = match resp {
                Ok(r) if r.status().is_success() => {
                    return Ok(r.json::<Response>().await?);
                }
                Ok(r) => {
                    let status = r.status();
                    let server_wait = retry_after(r.headers());
                    let detail = r.text().await.unwrap_or_default();
                    let detail = detail.chars().take(300).collect::<String>();
                    if !retryable(status) {
                        anyhow::bail!("typesafe {status}: {detail}");
                    }
                    tracing::warn!("typesafe {status} (attempt {attempt}); backing off");
                    last_err = Some(anyhow::anyhow!("typesafe {status}: {detail}"));
                    server_wait.unwrap_or(backoff)
                }
                Err(e) => {
                    tracing::warn!("typesafe request failed (attempt {attempt}): {e}");
                    last_err = Some(e.into());
                    backoff
                }
            };
            if attempt == MAX_ATTEMPTS {
                break;
            }
            // A retry that cannot finish before the deadline only makes the
            // user wait for the same failure.
            if Instant::now() + wait >= deadline {
                tracing::warn!("typesafe: not retrying, deadline would pass first");
                break;
            }
            tokio::time::sleep(wait).await;
            backoff *= 3;
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("typesafe: all attempts failed")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: serde_json::Value) -> Response {
        serde_json::from_value(v).expect("response should deserialize")
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

    #[test]
    fn retry_after_seconds_only() {
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&h), None);
        h.insert(reqwest::header::RETRY_AFTER, "2".parse().unwrap());
        assert_eq!(retry_after(&h), Some(Duration::from_secs(2)));
        h.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&h), None);
    }

    #[test]
    fn transient_statuses_are_the_retryable_ones() {
        use reqwest::StatusCode;
        for s in [429u16, 500, 502, 503, 529] {
            assert!(retryable(StatusCode::from_u16(s).unwrap()), "{s}");
        }
        for s in [400u16, 401, 404, 422] {
            assert!(!retryable(StatusCode::from_u16(s).unwrap()), "{s}");
        }
    }
}
