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
//! When the focused text field can be read over AT-SPI, the model is also
//! told what comes before the cursor, so the dictation continues it in the
//! same language and register; see [`TextContext`].
//!
//! The same model applies a spoken instruction to the text just dictated
//! ("make that more formal"); see [`Flow::edit`].
//!
//! A word the user changes in the field right after a dictation is a
//! correction parla can learn from; [`Flow::learn`] records those pairs
//! and, when the config says so, moves them into the dictionary.

use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use parla_flow::learned::{self, Suggestions};
use parla_flow::{paths, AppProfile, AppProfiles, Dictionary, History, Record, Snippets};

use crate::config::{FlowBackend, FlowConfig, LearnMode};
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

/// How much of the text before the cursor the model is shown.
const CONTEXT_CHARS: usize = 300;

/// The text field a dictation lands in: where the cursor is and what is
/// around it. The cleanup model is told what comes before the cursor;
/// what comes after only decides whether the text needs a space at its end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextContext {
    /// The application's name, as its toolkit reports it.
    pub app: String,
    /// The AT-SPI role name of the field ("text", "entry", "terminal").
    pub role: String,
    /// The text before the cursor, as much of it as was read.
    pub before: String,
    /// The text after the cursor, as much of it as was read.
    pub after: String,
}

impl TextContext {
    /// A context for a field, or None for a password field: what is in
    /// one never reaches a prompt.
    pub fn new(app: &str, role: &str, password: bool, before: &str, after: &str) -> Option<Self> {
        if password {
            return None;
        }
        Some(Self {
            app: app.to_string(),
            role: role.to_string(),
            before: before.to_string(),
            after: after.to_string(),
        })
    }

    /// From a field read over AT-SPI.
    pub fn from_focused(t: &desktopd::FocusedText) -> Option<Self> {
        Self::new(&t.app, &t.role, t.password, &t.before, &t.after)
    }
}

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
    /// Tell the model what is before the cursor, when it can be read.
    context: bool,
    learn: LearnMode,
    learn_after: Duration,
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

/// One correction [`Flow::learn`] took note of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Learned {
    pub heard: String,
    pub written: String,
    /// Times seen so far, this one included.
    pub count: u32,
    /// Moved into the dictionary (`flow.learn = "auto"`).
    pub promoted: bool,
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
            context: cfg.context,
            learn: cfg.learn,
            learn_after: Duration::from_millis(cfg.learn_after_ms),
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

    /// Whether the text around the cursor is read and given to the model.
    pub fn context_enabled(&self) -> bool {
        self.context
    }

    /// Whether corrections are looked for after a dictation.
    pub fn learn_enabled(&self) -> bool {
        self.learn != LearnMode::Off
    }

    /// How long after a dictation the field is read again.
    pub fn learn_after(&self) -> Duration {
        self.learn_after
    }

    /// Take note of corrections seen after a dictation into `app`. A pair
    /// the dictionary already covers, or the user dismissed, is skipped.
    /// With `flow.learn = "auto"` a pair seen twice goes into the
    /// dictionary (its written form as a word, the pair as a replacement)
    /// and the files are reloaded. Returns what was recorded.
    pub fn learn(&self, pairs: &[(String, String)], app: &str) -> anyhow::Result<Vec<Learned>> {
        if pairs.is_empty() || self.learn == LearnMode::Off {
            return Ok(Vec::new());
        }
        let path = paths::learned();
        let mut suggestions = Suggestions::load(&path)?;
        let mut dictionary = Dictionary::load(&paths::dictionary())?;
        // A pair the dictionary produces by now (the UI accepted it, or
        // the user wrote the word in by hand) has no business staying on
        // the list, or coming back with the save below.
        let before = suggestions.suggestions.len();
        suggestions
            .suggestions
            .retain(|s| !learned::covered(&dictionary, &s.heard, &s.written));
        let pruned = suggestions.suggestions.len() < before;
        let mut out = Vec::new();
        let mut promoted = false;
        for (heard, written) in pairs {
            if learned::covered(&dictionary, heard, written)
                || suggestions.is_dismissed(heard, written)
            {
                continue;
            }
            let count = suggestions.record(heard, written, app);
            if count == 0 {
                continue;
            }
            let mut l = Learned {
                heard: heard.clone(),
                written: written.clone(),
                count,
                promoted: false,
            };
            if self.learn == LearnMode::Auto && count >= 2 {
                if let Some(r) = suggestions.accept(&learned::key(heard, written)) {
                    learned::add_to_dictionary(&mut dictionary, r);
                    l.promoted = true;
                    promoted = true;
                }
            }
            out.push(l);
        }
        if out.is_empty() && !pruned {
            return Ok(out);
        }
        suggestions.save(&path)?;
        if promoted {
            dictionary.save(&paths::dictionary())?;
            self.reload()?;
        }
        Ok(out)
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
    /// With `flow.context` on, the model is told what is before the cursor
    /// (not for the code tone: a terminal's screen is not prose to
    /// continue). Whenever the field could be read, the result gets a
    /// space at either end where it would otherwise run into the word
    /// before or after the cursor; a snippet as much as any text.
    pub async fn process(
        &self,
        transcript: &str,
        class: &str,
        context: Option<&TextContext>,
    ) -> Processed {
        let seen = context.filter(|_| self.context);
        let mut out = self.process_inner(transcript, class, seen).await;
        if let Some(ctx) = context {
            let code = self.profile_for(class).tone == parla_flow::Tone::Code;
            out.text = spaced(out.text, &ctx.before, &ctx.after, code);
        }
        out
    }

    async fn process_inner(
        &self,
        transcript: &str,
        class: &str,
        context: Option<&TextContext>,
    ) -> Processed {
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
        let context = context.filter(|_| profile.tone != parla_flow::Tone::Code);
        let system = cleanup_prompt(&profile, &words, context);
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
            "learned": paths::learned(),
        })
    }
}

fn cleanup_prompt(profile: &AppProfile, words: &[String], context: Option<&TextContext>) -> String {
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
    // Last, so the local model's KV cache keeps the part that never changes.
    if let Some(ctx) = context {
        system.push_str("\n\n");
        system.push_str(&context_section(ctx));
    }
    system
}

/// The part of the prompt that says where the text is going.
fn context_section(ctx: &TextContext) -> String {
    let role = if ctx.role.trim().is_empty() {
        "text field"
    } else {
        ctx.role.trim()
    };
    let article = if role.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    let mut s = format!("The cursor is in {article} {role}");
    if !ctx.app.trim().is_empty() {
        s.push_str(&format!(" in {}", ctx.app.trim()));
    }
    let tail = last_chars(&ctx.before, CONTEXT_CHARS);
    if tail.trim().is_empty() {
        s.push_str(". The field is empty before the cursor.");
    } else {
        s.push_str(&format!(
            ". The text before the cursor ends with: \"{tail}\". Continue that text: match its \
language, register and capitalisation; if it ends mid-sentence, do not start with a capital; \
do not repeat any of it."
        ));
    }
    s
}

fn last_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// `text` with the spaces it needs to sit between `before` and `after`
/// without running into either; see [`needs_space`], which is applied at
/// both ends (the text is what is "before" the text after the cursor).
fn spaced(mut text: String, before: &str, after: &str, code: bool) -> String {
    if needs_space(before, &text, code) {
        text.insert(0, ' ');
    }
    if needs_space(&text, after, code) {
        text.push(' ');
    }
    text
}

/// Whether `text`, typed at the cursor, needs a space first so it does not
/// run into what is there. No space after whitespace, a line break, an
/// opening bracket or quote, or into an empty field, and none when the
/// text itself starts with whitespace or punctuation. With `code` on, no
/// space after any other symbol either, so "path/" + "parla" stays one path.
fn needs_space(before: &str, text: &str, code: bool) -> bool {
    let Some(last) = before.chars().next_back() else {
        return false;
    };
    let Some(first) = text.chars().next() else {
        return false;
    };
    let symbol = |c: char| !c.is_alphanumeric() && !c.is_whitespace();
    if last.is_whitespace() || "([{<\"'“‘«".contains(last) {
        return false;
    }
    if first.is_whitespace() || symbol(first) {
        return false;
    }
    if code && symbol(last) && !".,;:!?)]}".contains(last) {
        return false;
    }
    true
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
        let s = cleanup_prompt(&p, &["KWin".into(), "parla".into()], None);
        assert!(s.contains("terminal or code editor"));
        assert!(s.ends_with("KWin, parla."));
        assert!(s.contains("Prefer snake_case."));
        let s = cleanup_prompt(&AppProfile::fallback(), &[], None);
        assert!(!s.contains("Spell these"));
    }

    #[test]
    fn cleanup_prompt_describes_the_text_before_the_cursor() {
        let ctx = TextContext {
            app: "Thunderbird".into(),
            role: "entry".into(),
            before: "Hi Alan,\n\nthanks for the".into(),
            after: String::new(),
        };
        let s = cleanup_prompt(&AppProfile::fallback(), &["KWin".into()], Some(&ctx));
        assert!(s.contains("Spell these"));
        assert!(
            s.contains("KWin.\n\nThe cursor is in an entry in Thunderbird. The text before"),
            "{s}"
        );
        assert!(
            s.contains("ends with: \"Hi Alan,\n\nthanks for the\". Continue that text"),
            "{s}"
        );
        assert!(s.ends_with("do not repeat any of it."));
        // only the tail of a long field is quoted
        let long = TextContext {
            before: "x".repeat(1000),
            ..ctx.clone()
        };
        let s = cleanup_prompt(&AppProfile::fallback(), &[], Some(&long));
        assert!(s.contains(&format!("\"{}\"", "x".repeat(300))));
        assert!(!s.contains(&"x".repeat(301)));
        // an empty field is said to be empty
        let empty = TextContext {
            before: String::new(),
            ..ctx
        };
        let s = cleanup_prompt(&AppProfile::fallback(), &[], Some(&empty));
        assert!(s.ends_with("The field is empty before the cursor."), "{s}");
        assert!(s.contains("in an entry in Thunderbird"));
    }

    #[test]
    fn password_fields_give_no_context() {
        assert_eq!(
            TextContext::new("Firefox", "password text", true, "hunter2", ""),
            None
        );
        let ctx = TextContext::new("Firefox", "entry", false, "hello", "there").unwrap();
        assert_eq!(ctx.before, "hello");
        assert_eq!(ctx.after, "there");
    }

    #[test]
    fn leading_space_rule() {
        assert!(needs_space("Hello", "world", false));
        assert!(needs_space("Hello.", "World", false));
        assert!(!needs_space("Hello ", "world", false));
        assert!(!needs_space("Hello\n", "world", false));
        assert!(!needs_space("", "world", false));
        assert!(!needs_space("Hello", "", false));
        assert!(!needs_space("Hello", ", world", false));
        assert!(!needs_space("Hello", " world", false));
        assert!(!needs_space("say (", "hello", false));
        assert!(!needs_space("say \"", "hello", false));
        // code: symbols join, closers do not
        assert!(needs_space("echo foo", "bar", true));
        assert!(!needs_space("cd ~/parla/", "crates", true));
        assert!(!needs_space("git commit -m \"", "fix", true));
        assert!(needs_space("ls;", "cd parla", true));
        assert!(needs_space("a/", "b", false));
    }

    #[test]
    fn spaces_go_where_the_text_would_run_into_a_word() {
        assert_eq!(spaced("new".into(), "Hello ", "world", false), "new ");
        assert_eq!(spaced("new".into(), "Hello", "world", false), " new ");
        assert_eq!(spaced("new".into(), "Hello ", "", false), "new");
        assert_eq!(spaced("new".into(), "Hello ", ", world", false), "new");
        assert_eq!(spaced("new".into(), "Hello ", "\nworld", false), "new");
        assert_eq!(spaced("new ".into(), "Hello ", "world", false), "new ");
        assert_eq!(spaced("say \"".into(), "", "hello", false), "say \"");
        // code: the same symmetry as in front
        assert_eq!(
            spaced("crates/".into(), "cd ~/parla/", "flow", true),
            "crates/"
        );
        assert_eq!(spaced("cd parla".into(), "", "; ls", true), "cd parla");
        assert_eq!(spaced("ls".into(), "", "-la", true), "ls");
    }
}
