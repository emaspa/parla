//! Minimal TypeSafe System One client (`POST /v1/systemone`): the cloud
//! backend of the judged path.
//!
//! There is no Rust SDK, so this is the HTTP API directly: one request carries
//! the state plus every question, the model answers them independently and in
//! parallel, and answers come back under the ids we chose. Retries transient
//! failures with backoff under one overall deadline, which is the one thing
//! the official SDKs add. The question and answer types are the shared ones
//! in [`crate::oracle`]; their serde attributes are this API's wire format.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

use crate::oracle::{Question, Response};

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

const MAX_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);

#[derive(Debug, Serialize)]
struct Request<'a> {
    state: &'a serde_json::Value,
    model: &'a str,
    questions: &'a BTreeMap<String, Question>,
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

    pub fn model(&self) -> &str {
        &self.model
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
