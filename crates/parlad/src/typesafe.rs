//! Minimal TypeSafe System One client (`POST /v1/systemone`).
//!
//! There is no Rust SDK, so this is the HTTP API directly: one request carries
//! the state plus every question, the model answers them independently and in
//! parallel, and answers come back under the ids we chose. Retries on 429/529
//! with backoff, which is the one thing the official SDKs add.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

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
    /// One option from a defined set (max 255). Always give it a no-match
    /// option when the set may not cover the input.
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
    pub usage: Usage,
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
}

impl Client {
    pub fn new(api_key: String, model: String, timeout: Duration) -> anyhow::Result<Self> {
        anyhow::ensure!(!api_key.trim().is_empty(), "empty TypeSafe API key");
        Ok(Self {
            http: reqwest::Client::builder().timeout(timeout).build()?,
            api_key,
            model,
        })
    }

    /// Evaluate every question against one state. Retries 429/529 with
    /// exponential backoff; 401/422 fail immediately since retrying a
    /// malformed request or a bad key never helps.
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
        let mut backoff = Duration::from_millis(200);
        let mut last_err = None;

        for attempt in 1..=3 {
            let resp = self
                .http
                .post(ENDPOINT)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    return Ok(r.json::<Response>().await?);
                }
                Ok(r) => {
                    let status = r.status();
                    let detail = r.text().await.unwrap_or_default();
                    let detail = detail.chars().take(300).collect::<String>();
                    if status.as_u16() == 429 || status.as_u16() == 529 {
                        tracing::warn!("typesafe {status} (attempt {attempt}); backing off");
                        last_err = Some(anyhow::anyhow!("typesafe {status}: {detail}"));
                    } else {
                        anyhow::bail!("typesafe {status}: {detail}");
                    }
                }
                Err(e) => {
                    tracing::warn!("typesafe request failed (attempt {attempt}): {e}");
                    last_err = Some(e.into());
                }
            }
            if attempt < 3 {
                tokio::time::sleep(backoff).await;
                backoff *= 3;
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("typesafe: all attempts failed")))
    }
}
