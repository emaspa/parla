//! The cloud backend for dictation cleanup: any server that speaks the
//! OpenAI chat completions API. One request, temperature zero, the reply's
//! text. Used only when `[flow] backend = "openai"`.

use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::local::Generated;
use crate::oracle::Usage;

pub struct Client {
    http: reqwest::Client,
    url: String,
    api_key: String,
    model: String,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: [Message<'a>; 2],
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ReplyUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ReplyMessage,
}

#[derive(Deserialize)]
struct ReplyMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct ReplyUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl Client {
    pub fn new(
        base_url: &str,
        api_key: String,
        model: String,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            http,
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key,
            model,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn generate(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> anyhow::Result<Generated> {
        let body = Request {
            model: &self.model,
            messages: [
                Message {
                    role: "system",
                    content: system,
                },
                Message {
                    role: "user",
                    content: user,
                },
            ],
            temperature: 0.0,
            max_tokens,
        };
        let mut req = self.http.post(&self.url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().await.context("chat completions request")?;
        let status = resp.status();
        let text = resp.text().await.context("reading the reply")?;
        anyhow::ensure!(
            status.is_success(),
            "chat completions returned {status}: {}",
            text.chars().take(300).collect::<String>()
        );
        let reply: Reply = serde_json::from_str(&text).context("parsing the reply")?;
        let content = reply
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| anyhow::anyhow!("reply carried no message"))?;
        let usage = reply.usage.map_or(
            Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            |u| Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
            },
        );
        Ok(Generated {
            text: content,
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_without_usage_still_yields_text() {
        let r: Reply = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":"Hello."}}]}"#,
        )
        .unwrap();
        assert_eq!(r.choices[0].message.content.as_deref(), Some("Hello."));
        assert!(r.usage.is_none());
    }

    #[test]
    fn url_joins_without_double_slash() {
        let c = Client::new(
            "http://localhost:8080/v1/",
            String::new(),
            "m".into(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(c.url, "http://localhost:8080/v1/chat/completions");
    }
}
