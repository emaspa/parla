//! Dictation: from what whisper heard to what gets typed.
//!
//! A transcript goes through, in order: the snippets (an utterance that is
//! a snippet's name types the snippet and nothing else), the dictionary's
//! spoken-to-written replacements, and, for applications whose profile asks
//! for it, the cleanup model. Cleanup takes out filler words and false
//! starts, applies the speaker's own corrections, fixes punctuation, and
//! writes in the register the application's profile names. It never adds
//! content, and when it fails or times out the raw transcript is typed
//! instead, so a slow model costs latency, never words.
//!
//! The same model applies a spoken instruction to the text just dictated
//! ("make that more formal"); see [`Flow::edit`].

use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use parla_flow::{paths, AppProfile, AppProfiles, Dictionary, History, Record, Snippets};

use crate::config::{FlowBackend, FlowConfig};
use crate::local::{Generated, LocalModel};
use crate::openai;

const CLEANUP_PROMPT: &str =
    "You clean up text that a person dictated by voice, so it can be typed \
where they were writing. Output the cleaned text and nothing else: no preamble, no quotes \
around it, no explanation.

Rules:
- Remove filler words and false starts: um, uh, er, hmm, like, you know, I mean, sort of, \
basically, actually, so at the start of a sentence.
- When the speaker corrects themselves (\"send it Monday, no, Tuesday\"; \"make that three, \
I mean four\"; \"scratch that\"), keep only the final version.
- Fix punctuation and capitalisation. Spoken punctuation becomes the symbol: \"period\", \
\"comma\", \"question mark\", \"exclamation mark\", \"colon\", \"open quote\"; \"new line\" \
becomes a line break and \"new paragraph\" a blank line.
- When the speaker clearly enumerates items, lay them out as a list.
- Keep the meaning, the language and the speaker's own wording. Never answer, summarise, \
translate, or add anything that was not said.
- Plain text only: no markdown headings, bold or code fences. A list is lines starting with \
\"- \" or the numbers the speaker used.
- If the text is already clean, return it unchanged.";

const EDIT_PROMPT: &str = "You edit text for a person who dictates by voice. You are given the \
current text and an instruction they spoke. Apply the instruction to the text and output only \
the resulting text: no preamble, no quotes around it, no explanation. Keep everything the \
instruction does not ask you to change. If the instruction asks a question or is not about \
the text, output the text unchanged.";

/// Who rewrites dictation.
pub enum Rewriter {
    Local(Arc<LocalModel>),
    OpenAi(openai::Client),
}

impl Rewriter {
    async fn generate(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> anyhow::Result<Generated> {
        match self {
            Rewriter::Local(m) => m.generate(system, user, max_tokens, timeout).await,
            Rewriter::OpenAi(c) => c.generate(system, user, max_tokens).await,
        }
    }

    fn describe(&self) -> String {
        match self {
            Rewriter::Local(m) => format!("local {}", m.name()),
            Rewriter::OpenAi(c) => format!("openai {}", c.model()),
        }
    }
}

/// The three editable files, reloaded together.
struct Store {
    dictionary: Dictionary,
    snippets: Snippets,
    apps: AppProfiles,
}

impl Store {
    fn load() -> anyhow::Result<Self> {
        Ok(Self {
            dictionary: Dictionary::load(&paths::dictionary())?,
            snippets: Snippets::load(&paths::snippets())?,
            apps: AppProfiles::load(&paths::apps())?,
        })
    }
}

pub struct Flow {
    rewriter: Option<Rewriter>,
    timeout: Duration,
    max_tokens: u32,
    edit_window: Duration,
    store: RwLock<Store>,
    history: Option<Mutex<History>>,
    /// `asr.initial_prompt` from the config; the dictionary's words follow it.
    asr_prompt_base: Option<String>,
}

/// What a transcript became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    pub text: String,
    /// "snippet", "typed" (cleaned) or "raw" (cleanup off or failed).
    pub outcome: &'static str,
    /// Name of the profile that applied, empty for a snippet.
    pub profile: String,
}

impl Flow {
    /// Read the three files and build the rewriter. A missing file is an
    /// empty dictionary or snippet list, or the default profiles; a file
    /// that does not parse is an error, so a typo is noticed at startup and
    /// not by silently typing raw transcripts.
    pub fn load(
        cfg: &FlowConfig,
        asr_prompt_base: Option<String>,
        local: Option<Arc<LocalModel>>,
    ) -> anyhow::Result<Self> {
        let timeout = Duration::from_millis(cfg.timeout_ms);
        let rewriter = if !cfg.cleanup {
            None
        } else {
            Some(match cfg.backend {
                FlowBackend::Local => Rewriter::Local(local.ok_or_else(|| {
                    anyhow::anyhow!("flow.backend = \"local\" but no local model")
                })?),
                FlowBackend::OpenAi => Rewriter::OpenAi(openai::Client::new(
                    &cfg.openai.base_url,
                    cfg.openai.resolved_api_key(),
                    cfg.openai.model.clone(),
                    timeout,
                )?),
            })
        };
        let store = Store::load()?;
        tracing::info!(
            "dictionary: {} words, {} replacements; {} snippets; {} app profiles",
            store.dictionary.words.len(),
            store.dictionary.replacements.len(),
            store.snippets.snippets.len(),
            store.apps.apps.len()
        );
        Ok(Self {
            rewriter,
            timeout,
            max_tokens: cfg.max_tokens,
            edit_window: Duration::from_millis(cfg.edit_window_ms),
            store: RwLock::new(store),
            history: cfg
                .history
                .then(|| Mutex::new(History::open(paths::history()))),
            asr_prompt_base,
        })
    }

    /// Re-read the files. On a parse error the previous contents stay in
    /// force and the error names the file.
    pub fn reload(&self) -> anyhow::Result<()> {
        let fresh = Store::load()?;
        *self.store.write().unwrap_or_else(PoisonError::into_inner) = fresh;
        tracing::info!("reloaded dictionary, snippets and app profiles");
        Ok(())
    }

    /// The cleanup backend for a log line, or "off".
    pub fn describe(&self) -> String {
        self.rewriter
            .as_ref()
            .map_or_else(|| "off".into(), Rewriter::describe)
    }

    pub fn cleanup_enabled(&self) -> bool {
        self.rewriter.is_some()
    }

    /// How long after a dictation a spoken edit still refers to it.
    pub fn edit_window(&self) -> Duration {
        self.edit_window
    }

    /// What whisper is primed with: the configured prompt, then the
    /// dictionary's words, so names are transcribed as they are spelled.
    pub fn asr_prompt(&self) -> Option<String> {
        let store = self.store.read().unwrap_or_else(PoisonError::into_inner);
        match (&self.asr_prompt_base, store.dictionary.asr_prompt()) {
            (None, None) => None,
            (Some(b), None) => Some(b.clone()),
            (None, Some(w)) => Some(w),
            (Some(b), Some(w)) => Some(format!("{b} {w}")),
        }
    }

    pub fn profile_for(&self, class: &str) -> AppProfile {
        self.store
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .apps
            .for_class(class)
    }

    /// Turn a transcript into what gets typed into a window of `class`.
    pub async fn process(&self, transcript: &str, class: &str) -> Processed {
        let (snippet, replaced, profile, words) = {
            let store = self.store.read().unwrap_or_else(PoisonError::into_inner);
            (
                store.snippets.expand(transcript).map(|s| s.text.clone()),
                store.dictionary.apply_replacements(transcript),
                store.apps.for_class(class),
                store.dictionary.words.clone(),
            )
        };
        if let Some(text) = snippet {
            return Processed {
                text,
                outcome: "snippet",
                profile: String::new(),
            };
        }
        let raw = || Processed {
            text: spoken_breaks(&replaced),
            outcome: "raw",
            profile: profile.name.clone(),
        };
        let Some(rewriter) = self.rewriter.as_ref().filter(|_| profile.cleanup) else {
            return raw();
        };
        let system = cleanup_prompt(&profile, &words);
        let t0 = std::time::Instant::now();
        match rewriter
            .generate(&system, &replaced, self.max_tokens, self.timeout)
            .await
        {
            Ok(g) => match accept(&replaced, &g.text) {
                Some(text) => {
                    tracing::info!(
                        "cleaned up in {:.0}ms ({} in / {} out tokens, profile {:?}): {text:?}",
                        t0.elapsed().as_secs_f64() * 1000.0,
                        g.usage.input_tokens,
                        g.usage.output_tokens,
                        profile.name
                    );
                    let dictionary = &self
                        .store
                        .read()
                        .unwrap_or_else(PoisonError::into_inner)
                        .dictionary;
                    Processed {
                        text: dictionary.apply_replacements(&text),
                        outcome: "typed",
                        profile: profile.name.clone(),
                    }
                }
                None => {
                    tracing::warn!(
                        "cleanup produced {:?}; typing the transcript instead",
                        g.text
                    );
                    raw()
                }
            },
            Err(e) => {
                tracing::warn!("cleanup failed ({e:#}); typing the transcript instead");
                raw()
            }
        }
    }

    /// Apply a spoken instruction to `text`. Unlike [`Flow::process`] this
    /// has no fallback: an edit that failed is reported, not typed.
    pub async fn edit(&self, text: &str, instruction: &str, class: &str) -> anyhow::Result<String> {
        let rewriter = self
            .rewriter
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("voice edits need flow.cleanup = true"))?;
        let (profile, words) = {
            let store = self.store.read().unwrap_or_else(PoisonError::into_inner);
            (store.apps.for_class(class), store.dictionary.words.clone())
        };
        let mut system = EDIT_PROMPT.to_string();
        if profile.tone == parla_flow::Tone::Code {
            system.push_str("\n\nThe text is code or a command line: keep it literal.");
        }
        if !words.is_empty() {
            system.push_str(&format!(
                "\n\nSpell these names and terms exactly so: {}.",
                words.join(", ")
            ));
        }
        let user = format!("Text:\n{text}\n\nInstruction: {instruction}");
        let g = rewriter
            .generate(&system, &user, self.max_tokens, self.timeout)
            .await
            .context("edit model")?;
        let out = tidy_lines(&unquote(g.text.trim(), text));
        anyhow::ensure!(!out.is_empty(), "the model returned nothing");
        Ok(out)
    }

    /// Append to the history, if kept. Returns the stored record with its id.
    pub fn record(&self, record: Record) -> Option<Record> {
        let history = self.history.as_ref()?;
        match history
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .append(record)
        {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!("history: {e:#}");
                None
            }
        }
    }

    /// The history store, for the UI's queries; None when history is off.
    pub fn history(&self) -> Option<&Mutex<History>> {
        self.history.as_ref()
    }

    /// The files the UI may edit or show.
    pub fn paths(&self) -> serde_json::Value {
        serde_json::json!({
            "config": crate::config::DaemonConfig::path(),
            "dictionary": paths::dictionary(),
            "snippets": paths::snippets(),
            "apps": paths::apps(),
            "history": paths::history(),
        })
    }
}

fn cleanup_prompt(profile: &AppProfile, words: &[String]) -> String {
    let mut system = CLEANUP_PROMPT.to_string();
    system.push_str("\n\n");
    system.push_str(profile.tone.guidance());
    let extra = profile.instructions.trim();
    if !extra.is_empty() {
        system.push(' ');
        system.push_str(extra);
    }
    if !words.is_empty() {
        system.push_str(&format!(
            "\n\nSpell these names and terms exactly so, even if they were transcribed differently: {}.",
            words.join(", ")
        ));
    }
    system
}

/// The cleaned text, or None when the model's output cannot be trusted to
/// be a cleanup of `raw`: empty, or long enough that it must have added
/// something.
fn accept(raw: &str, out: &str) -> Option<String> {
    let out = tidy_lines(&unquote(out.trim(), raw));
    if out.is_empty() {
        return None;
    }
    let limit = raw.chars().count() * 2 + 40;
    if out.chars().count() > limit {
        return None;
    }
    Some(out)
}

/// Trailing spaces off every line: markdown's two-space line breaks would
/// be typed literally.
fn tidy_lines(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip one pair of surrounding quotes the model added when `raw` had
/// none.
fn unquote(out: &str, raw: &str) -> String {
    let raw_quoted = raw.starts_with('"') || raw.starts_with('“');
    for (open, close) in [('"', '"'), ('“', '”'), ('«', '»')] {
        if !raw_quoted && out.starts_with(open) && out.ends_with(close) && out.chars().count() >= 2
        {
            let inner: String = out.chars().skip(1).collect();
            let inner: String = inner.chars().take(inner.chars().count() - 1).collect();
            return inner.trim().to_string();
        }
    }
    out.to_string()
}

/// "new line" and "new paragraph" as breaks, for text that skips the
/// model. Surrounding punctuation whisper added is dropped with them.
fn spoken_breaks(text: &str) -> String {
    let mut out = text.to_string();
    for (phrase, brk) in [("new paragraph", "\n\n"), ("new line", "\n")] {
        let lower = out.to_lowercase();
        if !lower.contains(phrase) {
            continue;
        }
        let words: Vec<&str> = phrase.split(' ').collect();
        let mut result = String::new();
        let mut rest = out.as_str();
        while let Some(pos) = rest.to_lowercase().find(phrase) {
            let before_ok = pos == 0
                || !rest[..pos]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric);
            let end = pos + phrase.len();
            let after_ok = end == rest.len()
                || !rest[end..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric);
            if !(before_ok && after_ok) || rest.len() != rest.to_lowercase().len() {
                result.push_str(&rest[..end]);
                rest = &rest[end..];
                continue;
            }
            let head = rest[..pos]
                .trim_end()
                .trim_end_matches(|c: char| ",.;:!?".contains(c));
            result.push_str(head);
            result.push_str(brk);
            rest = rest[end..].trim_start_matches(|c: char| ",.;:!? ".contains(c));
            let _ = &words;
        }
        result.push_str(rest);
        out = result;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_rejects_empty_and_runaway_output() {
        assert_eq!(accept("um hello", ""), None);
        assert_eq!(accept("a b", "a  \nb  "), Some("a\nb".into()));
        assert_eq!(accept("um hello", "  "), None);
        assert_eq!(accept("um hello", "Hello."), Some("Hello.".into()));
        let long = "x".repeat(200);
        assert_eq!(accept("um hello", &long), None);
    }

    #[test]
    fn model_quotes_are_stripped_unless_spoken() {
        assert_eq!(
            accept("hi there", "\"Hi there.\""),
            Some("Hi there.".into())
        );
        assert_eq!(accept("hi there", "“Hi there.”"), Some("Hi there.".into()));
        assert_eq!(
            accept("\"quoted\" she said", "\"Quoted,\" she said."),
            Some("\"Quoted,\" she said.".into())
        );
    }

    #[test]
    fn spoken_breaks_become_newlines() {
        assert_eq!(spoken_breaks("Hello. New line. Bye"), "Hello\nBye");
        assert_eq!(spoken_breaks("one new paragraph two"), "one\n\ntwo");
        assert_eq!(spoken_breaks("a newline b"), "a newline b");
        assert_eq!(spoken_breaks("plain text"), "plain text");
    }

    #[test]
    fn cleanup_prompt_carries_tone_and_words() {
        let p = AppProfile {
            tone: parla_flow::Tone::Code,
            instructions: "Prefer snake_case.".into(),
            ..AppProfile::default()
        };
        let s = cleanup_prompt(&p, &["KWin".into(), "parla".into()]);
        assert!(s.contains("terminal or code editor"));
        assert!(s.ends_with("KWin, parla."));
        assert!(s.contains("Prefer snake_case."));
        let s = cleanup_prompt(&AppProfile::fallback(), &[]);
        assert!(!s.contains("Spell these"));
    }
}
