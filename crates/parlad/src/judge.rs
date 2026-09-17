//! The judged path: what to do with an utterance the grammar rejected.
//!
//! `Grammar::parse` matches literal word sequences, so it answers in
//! microseconds and misses anything phrased differently ("bring up firefox",
//! "kill that window"). Rather than hand those to a conversational agent, this
//! asks one TypeSafe request for the intent *and* every argument that intent
//! might need — the questions are independent and evaluated in parallel, so
//! asking for arguments we will discard costs only their tokens.
//!
//! Code still owns everything code is good at: which apps are installed, which
//! windows are open, how many desktops exist, and what counts as confident
//! enough to act. The model only supplies the semantic step — which of the
//! candidates the user meant.

use std::collections::{BTreeMap, BTreeSet};

use parla_grammar::{DesktopIndex, Intent};
use serde_json::json;

use crate::config::TypeSafeConfig;
use crate::typesafe::{Client, NoulCriteria, Question, Response};
use desktopd::windows::Window;

/// Sentinel option names. Prefixed so they can never collide with a real app
/// name or window title.
const NO_TARGET: &str = "__none__";
const FOCUSED: &str = "__focused_window__";

/// How many installed apps to offer as candidates. The model cannot pick a
/// value we omit, so this is the one number that decides whether an app is
/// reachable by voice at all.
const APP_CANDIDATES: usize = 24;

/// What the judged path concluded.
#[derive(Debug)]
pub enum Judgment {
    /// Act on this. `confidence` is the weakest link across the judgments that
    /// built it; `needs_confirmation` folds in the risk judgment.
    Act {
        intent: Intent,
        confidence: f64,
        needs_confirmation: bool,
    },
    /// The user was dictating prose, not commanding — they are holding the
    /// wrong hotkey.
    Dictation,
    /// Not confidently anything. Carries a reason for the notification.
    Unclear(String),
}

pub struct Judge {
    client: Client,
    cfg: TypeSafeConfig,
}

/// Context code gathers before asking. Everything here is observed fact, kept
/// separate from anything the model infers.
pub struct Context<'a> {
    pub windows: &'a [Window],
    pub index: &'a DesktopIndex,
    pub current_desktop: u32,
    pub desktop_count: u32,
    pub claude_running: bool,
}

impl Judge {
    pub fn new(cfg: TypeSafeConfig) -> anyhow::Result<Self> {
        let key = cfg.resolved_api_key()?;
        let client = Client::new(
            key,
            cfg.model.clone(),
            std::time::Duration::from_millis(cfg.timeout_ms),
        )?;
        Ok(Self { client, cfg })
    }

    pub async fn judge(&self, utterance: &str, ctx: &Context<'_>) -> anyhow::Result<Judgment> {
        let candidates = Candidates::build(utterance, ctx);
        let state = json!({
            "utterance": utterance,
            "open_windows": ctx.windows.iter().map(|w| json!({
                "title": w.title,
                "application": w.class,
            })).collect::<Vec<_>>(),
            "current_desktop": ctx.current_desktop,
            "desktop_count": ctx.desktop_count,
            "claude_code_session_running": ctx.claude_running,
        });

        let questions = candidates.questions(ctx);
        let t0 = std::time::Instant::now();
        let resp = self.client.evaluate(&state, &questions).await?;
        tracing::info!(
            "judged {:?} in {:.0}ms ({} in / {} out tokens)",
            utterance,
            t0.elapsed().as_secs_f64() * 1000.0,
            resp.usage.input_tokens,
            resp.usage.output_tokens,
        );

        Ok(compose(&resp, &self.cfg, &candidates))
    }
}

/// Turn the answers into an intent. Policy lives here rather than in the
/// questions, so thresholds can change without re-running inference — and so
/// it can be tested against recorded answers without a network call.
fn compose(resp: &Response, cfg: &TypeSafeConfig, candidates: &Candidates) -> Judgment {
    if let Some(p) = resp.noul("is_dictation") {
        if p >= cfg.dictation_threshold {
            return Judgment::Dictation;
        }
    }

    let Some((name, intent_conf)) = resp.choice("intent") else {
        return Judgment::Unclear("model returned no intent".into());
    };
    if name == NO_TARGET {
        return Judgment::Unclear("not a desktop command".into());
    }
    if intent_conf < cfg.min_confidence {
        return Judgment::Unclear(format!(
            "unsure what {name:?} meant (confidence {intent_conf:.2})"
        ));
    }

    // Weakest link, not a product: one wrong argument spoils the action,
    // so the action is only as trustworthy as its least certain part.
    let mut confidence = intent_conf;
    let mut args: BTreeMap<String, String> = BTreeMap::new();

    // Required argument: its absence means we misread the intent.
    macro_rules! required {
        ($id:expr, $key:expr, $map:expr) => {
            match arg(resp, $id, &mut confidence) {
                Some(v) => args.insert($key.into(), $map(v)),
                None => {
                    return Judgment::Unclear(format!("{name} without a {}", $id));
                }
            }
        };
    }

    match name {
        "show_app" | "krunner" => {
            required!("target", "query", |v: String| v);
        }
        "close_window" | "minimize_window" | "maximize_window" => {
            // Optional query: no named target means the focused window,
            // which is a legitimate answer rather than a failure.
            if let Some(t) = arg(resp, "target", &mut confidence) {
                if t != FOCUSED {
                    args.insert("query".into(), t);
                }
            }
        }
        "virtual_desktop" => {
            required!("desktop_number", "n", |v: String| v);
        }
        "virtual_desktop_rel" => {
            required!("desktop_direction", "delta", |v: String| {
                if v == "previous" {
                    "-1".to_string()
                } else {
                    "1".to_string()
                }
            });
        }
        "start_claude" => {
            // Optional: absent leaves the configured default model.
            if let Some(m) = arg(resp, "claude_model", &mut confidence) {
                args.insert("model".into(), m);
            }
        }
        "claude_model" => {
            required!("claude_model", "model", |v: String| v);
        }
        "claude_tell" | "notify" => {
            required!("payload", "text", |v: String| v);
        }
        "key" => {
            required!("payload", "chord", |v: String| v.replace(" plus ", " "));
        }
        // open_terminal and claude_read take no arguments.
        _ => {}
    }

    // "make this visible" splits into launch-vs-focus on an observed fact, so
    // code decides it rather than spending model probability on the split.
    let name = match name {
        "show_app" => {
            let target = args.get("query").map(String::as_str).unwrap_or_default();
            if candidates.is_open_window(target) {
                "focus_window"
            } else {
                "launch_app"
            }
        }
        other => other,
    };

    let Some(intent) = Intent::from_args(name, &args) else {
        return Judgment::Unclear(format!("could not build {name} from {args:?}"));
    };

    // Confirmation gates on judged consequence rather than intent shape:
    // closing a terminal mid-build and closing a calculator are the same
    // variant. Being *confident* about a destructive request is not
    // permission to carry it out, so risk alone forces the prompt; low
    // confidence forces it even for harmless actions.
    let risky = resp
        .noul("is_destructive")
        .is_some_and(|p| p >= cfg.destructive_threshold);
    let needs_confirmation =
        risky || intent.needs_confirmation() || confidence < cfg.act_unconfirmed_above;

    Judgment::Act {
        intent,
        confidence,
        needs_confirmation,
    }
}

/// Read one chosen argument, folding its confidence into the running minimum.
/// A no-match answer yields None rather than a bogus value — the model cannot
/// invent an option we never offered.
fn arg(resp: &Response, id: &str, confidence: &mut f64) -> Option<String> {
    let (value, c) = resp.choice(id)?;
    if value == NO_TARGET {
        return None;
    }
    *confidence = confidence.min(c);
    Some(value.to_string())
}

/// Candidate values assembled by code, for the model to select among.
struct Candidates {
    /// Option name -> what it is, for the `target` choice.
    targets: BTreeMap<String, String>,
    /// Which of those options are windows that already exist.
    open_windows: BTreeSet<String>,
    /// Trailing spans of the utterance, for verbatim payload selection.
    spans: Vec<String>,
}

impl Candidates {
    fn is_open_window(&self, target: &str) -> bool {
        self.open_windows.contains(target)
    }

    #[cfg(test)]
    fn with_open_windows(titles: &[&str]) -> Self {
        Self {
            targets: BTreeMap::new(),
            open_windows: titles.iter().map(|t| (*t).to_string()).collect(),
            spans: Vec::new(),
        }
    }
}

impl Candidates {
    fn build(utterance: &str, ctx: &Context<'_>) -> Self {
        let mut targets = BTreeMap::new();
        let mut seen = BTreeSet::new();

        for e in ctx.index.shortlist(utterance, APP_CANDIDATES) {
            if seen.insert(e.name.to_lowercase()) {
                let desc = match &e.generic_name {
                    Some(g) => format!("installed application ({g})"),
                    None => "installed application".to_string(),
                };
                targets.insert(e.name.clone(), desc);
            }
        }
        let mut open_windows = BTreeSet::new();
        for w in ctx.windows {
            open_windows.insert(w.title.clone());
            if seen.insert(w.title.to_lowercase()) {
                targets.insert(
                    w.title.clone(),
                    format!("window that is already open, belonging to {}", w.class),
                );
            }
        }
        targets.insert(
            FOCUSED.into(),
            "the window that currently has focus, because the user named no target".into(),
        );
        targets.insert(
            NO_TARGET.into(),
            "the utterance names no application or window".into(),
        );

        // Every trailing span, so a payload can be selected verbatim instead
        // of regenerated. "tell claude to fix the test" -> "to fix the test".
        let words: Vec<&str> = utterance.split_whitespace().collect();
        let spans: Vec<String> = (0..words.len()).map(|i| words[i..].join(" ")).collect();

        Self {
            targets,
            open_windows,
            spans,
        }
    }

    fn questions(&self, ctx: &Context<'_>) -> BTreeMap<String, Question> {
        let mut q = BTreeMap::new();

        q.insert(
            "intent".into(),
            Question::Choice {
                instructions: json!({
                    "task": "The user spoke `utterance` to a voice assistant that drives a KDE Plasma desktop. Decide which single action they asked for.",
                    "note": "Use `open_windows` to tell a request to start something new from a request to switch to something already running.",
                }),
                criteria: intent_criteria(),
            },
        );

        q.insert(
            "is_dictation".into(),
            Question::Noul {
                instructions: json!("Is `utterance` prose the user wants typed verbatim into the focused window, rather than an instruction for the desktop to carry out?"),
                criteria: Some(NoulCriteria {
                    yes: "Text to be transcribed as-is, such as a sentence of an email, a chat message, or a code comment.".into(),
                    no: "An instruction to the desktop or to a tool: launching, focusing, closing, switching, searching, or telling Claude Code something.".into(),
                }),
            },
        );

        q.insert(
            "is_destructive".into(),
            Question::Noul {
                instructions: json!({
                    "question": "If the assistant carries out `utterance`, could that destroy unsaved work or be hard for the user to undo?",
                    "focus": "Judge the actual target in `open_windows`, not the verb alone.",
                }),
                criteria: Some(NoulCriteria {
                    yes: "Closing or killing something holding unsaved state or a long-running job: an editor with unsaved changes, a terminal running a build, a VM.".into(),
                    no: "Reversible or inert: focusing, minimizing, maximizing, switching desktop, launching, searching, notifying, or closing a viewer with no editable state.".into(),
                }),
            },
        );

        q.insert(
            "target".into(),
            Question::Choice {
                instructions: json!({
                    "task": "Assuming `utterance` acts on an application or window, which candidate did the user mean?",
                    "note": "Candidates are the applications installed on this machine and the windows currently open. Choose the open window when the user implies something already running.",
                }),
                criteria: self
                    .targets
                    .iter()
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect(),
            },
        );

        let mut desktops: BTreeMap<String, serde_json::Value> = (1..=ctx.desktop_count.max(1))
            .map(|n| (n.to_string(), json!(format!("virtual desktop number {n}"))))
            .collect();
        desktops.insert(
            NO_TARGET.into(),
            json!("no specific desktop number is named"),
        );
        q.insert(
            "desktop_number".into(),
            Question::Choice {
                instructions: json!("Assuming `utterance` asks to switch to a specific numbered virtual desktop, which number? The user is currently on `current_desktop`."),
                criteria: desktops,
            },
        );

        q.insert(
            "desktop_direction".into(),
            Question::Choice {
                instructions: json!("Assuming `utterance` asks to move one virtual desktop relative to the current one, in which direction?"),
                criteria: BTreeMap::from([
                    ("next".into(), json!("forward, to a higher-numbered desktop")),
                    ("previous".into(), json!("back, to a lower-numbered desktop")),
                    (NO_TARGET.into(), json!("no relative movement is asked for")),
                ]),
            },
        );

        q.insert(
            "claude_model".into(),
            Question::Choice {
                instructions: json!("Assuming `utterance` names which Claude model to use, which one? Choose the no-name option if the user does not mention a model at all, so the configured default stands."),
                criteria: BTreeMap::from([
                    ("opus".into(), json!("the most capable model; also 'the big one', 'the smart one'")),
                    ("sonnet".into(), json!("the balanced default")),
                    ("haiku".into(), json!("the smallest and fastest; also 'the quick one', 'the cheap one'")),
                    (NO_TARGET.into(), json!("no model is named or implied")),
                ]),
            },
        );

        let mut spans: BTreeMap<String, serde_json::Value> = self
            .spans
            .iter()
            .map(|s| (s.clone(), json!(null)))
            .collect();
        spans.insert(
            NO_TARGET.into(),
            json!("`utterance` carries no such message"),
        );
        q.insert(
            "payload".into(),
            Question::Choice {
                instructions: json!({
                    "task": "Assuming `utterance` carries a message to pass on verbatim — a prompt for Claude Code, a notification body, a search string, or a key chord — which candidate is exactly that message?",
                    "focus": "Candidates are the trailing spans of `utterance`. Pick the one that starts where the command wrapper ends, keeping the message itself complete and unaltered.",
                    "example": "For 'tell claude to fix the failing test', the message is 'to fix the failing test', not the whole utterance.",
                }),
                criteria: spans,
            },
        );

        q
    }
}

fn intent_criteria() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        // Launch-vs-focus is deliberately absent: whether the target already
        // runs is an observed fact, so code decides it and the model's
        // probability is not split between two spellings of one wish.
        (
            "show_app".into(),
            json!({
                "what": "Put an application or window in front of the user, whether or not it is already running",
                "examples": [
                    "open firefox", "bring up my editor",
                    "switch to the browser", "I need a terminal window",
                ],
            }),
        ),
        ("close_window".into(), json!("Close or quit a window")),
        ("minimize_window".into(), json!("Minimize or hide a window")),
        (
            "maximize_window".into(),
            json!("Maximize or full-screen a window"),
        ),
        (
            "virtual_desktop".into(),
            json!("Switch to a virtual desktop identified by number"),
        ),
        (
            "virtual_desktop_rel".into(),
            json!("Move one virtual desktop forward or back from the current one"),
        ),
        (
            "open_terminal".into(),
            json!("Open a terminal emulator, with no particular program named"),
        ),
        (
            "start_claude".into(),
            json!("Start a Claude Code coding session"),
        ),
        (
            "claude_model".into(),
            json!("Change which model the running Claude Code session uses"),
        ),
        (
            "claude_tell".into(),
            json!("Pass an instruction or prompt through to the running Claude Code session"),
        ),
        (
            "claude_read".into(),
            json!("Read back what the Claude Code session most recently output"),
        ),
        (
            "krunner".into(),
            json!("Search the system for a file or application by name"),
        ),
        (
            "notify".into(),
            json!("Show the user a desktop notification"),
        ),
        (
            "key".into(),
            json!("Send a raw keyboard chord to whatever window has focus"),
        ),
        (
            NO_TARGET.into(),
            json!("Not a desktop command, or too ambiguous to act on safely"),
        ),
    ])
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::Response;

    /// Recorded answers, exactly as the API returns them.
    pub fn answers(json: serde_json::Value) -> Response {
        serde_json::from_value(serde_json::json!({
            "model": "jev-latest",
            "answers": json,
            "usage": { "input_tokens": 0, "output_tokens": 0 },
        }))
        .expect("recorded answer should deserialize")
    }

    pub fn pick(c: &str, conf: f64) -> serde_json::Value {
        serde_json::json!({
            "type": "choice",
            "choice": c,
            "confidence": conf,
            "probabilities": { c: conf },
        })
    }

    pub fn yes_no(p: f64) -> serde_json::Value {
        serde_json::json!({ "type": "noul", "noul": p })
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{answers as resp, pick as choice, yes_no as noul};
    use super::*;

    fn cfg() -> TypeSafeConfig {
        TypeSafeConfig::default()
    }

    /// Nothing open, so `show_app` resolves to a launch.
    fn cand() -> Candidates {
        Candidates::with_open_windows(&[])
    }

    #[test]
    fn confident_launch_acts_without_confirmation() {
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.94),
            "target": choice("Firefox", 0.99),
            "is_dictation": noul(0.02),
            "is_destructive": noul(0.03),
        }));
        match compose(&r, &cfg(), &cand()) {
            Judgment::Act {
                intent,
                needs_confirmation,
                ..
            } => {
                assert_eq!(
                    intent,
                    Intent::LaunchApp {
                        query: "Firefox".into()
                    }
                );
                assert!(!needs_confirmation);
            }
            other => panic!("expected Act, got {other:?}"),
        }
    }

    #[test]
    fn confidence_is_the_weakest_link_not_the_product() {
        // A certain intent with a shaky argument is only as good as the
        // argument; multiplying would have given 0.47 and read as unclear.
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.95),
            "target": choice("Kate", 0.50),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.01),
        }));
        match compose(&r, &cfg(), &cand()) {
            Judgment::Act { confidence, .. } => assert_eq!(confidence, 0.50),
            other => panic!("expected Act, got {other:?}"),
        }
    }

    #[test]
    fn dictation_wins_over_any_intent() {
        // Prose that happens to read like a command must not be executed.
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.99),
            "target": choice("Firefox", 0.99),
            "is_dictation": noul(0.88),
            "is_destructive": noul(0.01),
        }));
        assert!(matches!(compose(&r, &cfg(), &cand()), Judgment::Dictation));
    }

    #[test]
    fn judged_risk_forces_confirmation_even_when_certain() {
        // Understanding the request perfectly is not permission to carry it
        // out: closing a terminal mid-build still asks.
        let r = resp(serde_json::json!({
            "intent": choice("close_window", 1.0),
            "target": choice("build — Konsole", 1.0),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.91),
        }));
        match compose(&r, &cfg(), &cand()) {
            Judgment::Act {
                needs_confirmation, ..
            } => assert!(needs_confirmation),
            other => panic!("expected Act, got {other:?}"),
        }
    }

    #[test]
    fn harmless_intent_judged_safe_skips_confirmation() {
        // The structural rule marks every close_window destructive; the
        // judged one lets a scratch viewer through.
        let r = resp(serde_json::json!({
            "intent": choice("minimize_window", 0.92),
            "target": choice("__focused_window__", 0.95),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.04),
        }));
        match compose(&r, &cfg(), &cand()) {
            Judgment::Act {
                intent,
                needs_confirmation,
                ..
            } => {
                assert_eq!(intent, Intent::MinimizeWindow { query: None });
                assert!(!needs_confirmation);
            }
            other => panic!("expected Act, got {other:?}"),
        }
    }

    #[test]
    fn no_match_and_low_confidence_both_refuse() {
        let none = resp(serde_json::json!({
            "intent": choice(NO_TARGET, 0.9),
            "is_dictation": noul(0.1),
        }));
        assert!(matches!(
            compose(&none, &cfg(), &cand()),
            Judgment::Unclear(_)
        ));

        let shaky = resp(serde_json::json!({
            "intent": choice("close_window", 0.20),
            "is_dictation": noul(0.1),
        }));
        assert!(matches!(
            compose(&shaky, &cfg(), &cand()),
            Judgment::Unclear(_)
        ));
    }

    #[test]
    fn missing_required_argument_refuses_rather_than_guessing() {
        // No desktop number was offered that fit, so there is nothing to act
        // on — better than switching to a desktop the user did not name.
        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop", 0.97),
            "desktop_number": choice(NO_TARGET, 0.99),
            "is_dictation": noul(0.01),
        }));
        assert!(matches!(compose(&r, &cfg(), &cand()), Judgment::Unclear(_)));
    }

    #[test]
    fn optional_model_absent_leaves_the_default() {
        let r = resp(serde_json::json!({
            "intent": choice("start_claude", 0.96),
            "claude_model": choice(NO_TARGET, 0.98),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.02),
        }));
        match compose(&r, &cfg(), &cand()) {
            Judgment::Act { intent, .. } => {
                assert_eq!(intent, Intent::StartClaude { model: None });
            }
            other => panic!("expected Act, got {other:?}"),
        }
    }

    #[test]
    fn payload_span_is_taken_verbatim() {
        let r = resp(serde_json::json!({
            "intent": choice("claude_tell", 0.93),
            "payload": choice("to rerun the failing test", 0.9),
            "is_dictation": noul(0.2),
            "is_destructive": noul(0.05),
        }));
        match compose(&r, &cfg(), &cand()) {
            Judgment::Act { intent, .. } => assert_eq!(
                intent,
                Intent::ClaudeTell {
                    text: "to rerun the failing test".into()
                }
            ),
            other => panic!("expected Act, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod show_app_tests {
    use super::tests_support::*;
    use super::*;

    #[test]
    fn show_app_becomes_launch_when_nothing_is_open() {
        let r = answers(serde_json::json!({
            "intent": pick("show_app", 0.9),
            "target": pick("Dolphin", 0.95),
            "is_dictation": yes_no(0.02),
            "is_destructive": yes_no(0.02),
        }));
        let cand = Candidates::with_open_windows(&["build — Konsole"]);
        match compose(&r, &TypeSafeConfig::default(), &cand) {
            Judgment::Act { intent, .. } => {
                assert_eq!(
                    intent,
                    Intent::LaunchApp {
                        query: "Dolphin".into()
                    }
                )
            }
            other => panic!("expected Act, got {other:?}"),
        }
    }

    #[test]
    fn show_app_becomes_focus_when_the_window_exists() {
        // Same answers, different observed state: code flips the verb, and no
        // model probability was spent on the distinction.
        let r = answers(serde_json::json!({
            "intent": pick("show_app", 0.9),
            "target": pick("build — Konsole", 0.95),
            "is_dictation": yes_no(0.02),
            "is_destructive": yes_no(0.02),
        }));
        let cand = Candidates::with_open_windows(&["build — Konsole"]);
        match compose(&r, &TypeSafeConfig::default(), &cand) {
            Judgment::Act { intent, .. } => assert_eq!(
                intent,
                Intent::FocusWindow {
                    query: "build — Konsole".into()
                }
            ),
            other => panic!("expected Act, got {other:?}"),
        }
    }
}
