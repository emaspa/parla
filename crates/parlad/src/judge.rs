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
//! candidates the user meant. Every candidate is offered under an opaque key
//! (`a:3`, `w:0`, `p:2`) with the human-readable value in its description, so
//! a window titled `__none__` can never be mistaken for the no-match option,
//! and every returned choice is checked against what was offered.

use std::collections::{BTreeMap, BTreeSet};

use parla_grammar::{DesktopIndex, Intent};
use serde_json::json;

use crate::config::TypeSafeConfig;
use crate::router::policy::{Decision, Policy, Signals};
use crate::typesafe::{Client, NoulCriteria, Question, Response, MAX_CHOICE_OPTIONS};
use desktopd::windows::Window;

/// Sentinel option names. Real candidates use `a:`/`w:`/`p:` keys, so these
/// cannot collide with anything observed.
const NO_TARGET: &str = "__none__";
const FOCUSED: &str = "__focused_window__";

/// Opaque key prefixes for observed candidates.
const APP_KEY: &str = "a:";
const WINDOW_KEY: &str = "w:";
const SPAN_KEY: &str = "p:";

/// How many installed apps to offer as candidates. The model cannot pick a
/// value we omit, so this is the one number that decides whether an app is
/// reachable by voice at all.
const APP_CANDIDATES: usize = 24;
/// How many open windows to offer. Windows come first in the list kdotool
/// returns, so a cluttered desktop loses its oldest windows, not its newest.
const WINDOW_CANDIDATES: usize = 96;
/// How many trailing words of the utterance may start a payload. Bounds both
/// the option count and the total text sent (each span is at most the
/// utterance's tail, so the sum is linear in this constant).
const PAYLOAD_SPANS: usize = 64;

/// How many numbered desktops to offer. KWin itself stops at 20, so this
/// only matters for keeping the question under the API's limit.
const DESKTOP_CANDIDATES: u32 = (MAX_CHOICE_OPTIONS - 1) as u32;

const _: () = assert!(APP_CANDIDATES + WINDOW_CANDIDATES + 2 <= MAX_CHOICE_OPTIONS);
const _: () = assert!(PAYLOAD_SPANS < MAX_CHOICE_OPTIONS);

/// An intent the judged path built, plus the signals the policy needs to
/// decide whether it may run. The decision itself is the router's, so both
/// paths go through one gate.
#[derive(Debug)]
pub struct Resolved {
    pub intent: Intent,
    /// KWin id of the window the user named, when the target was chosen from
    /// the open-window list. The `Intent` carries the title as a query for
    /// now; an executor that takes ids should prefer this.
    pub window_id: Option<String>,
    /// Weakest link across the judgments that built the intent.
    pub confidence: f64,
    /// What the model said about risk and dictation, for [`Policy::decide`].
    pub signals: Signals,
}

/// What the judged path concluded.
#[derive(Debug)]
pub enum Verdict {
    /// An intent was built. Whether it runs, asks, or is refused is
    /// [`Policy::decide`]'s call.
    Act(Resolved),
    /// The user was dictating prose, not commanding — they are holding the
    /// wrong hotkey.
    Dictation,
    /// Not confidently anything. Carries a reason for the notification.
    Unclear(String),
}

/// Flattened view of a [`Verdict`], kept for `parlad --judge` until main.rs
/// adopts `Verdict` (it loses the window id and the confirmation reason).
#[derive(Debug)]
pub enum Judgment {
    Act {
        intent: Intent,
        confidence: f64,
        needs_confirmation: bool,
    },
    Dictation,
    Unclear(String),
}

impl Verdict {
    /// Apply the policy and flatten: a refusal becomes `Unclear`.
    pub fn flatten(self, policy: &Policy) -> Judgment {
        match self {
            Verdict::Act(r) => match policy.decide(&r.intent, &r.signals) {
                Decision::Refuse { reason } => Judgment::Unclear(reason),
                decision => Judgment::Act {
                    intent: r.intent,
                    confidence: r.confidence,
                    needs_confirmation: matches!(decision, Decision::Confirm { .. }),
                },
            },
            Verdict::Dictation => Judgment::Dictation,
            Verdict::Unclear(s) => Judgment::Unclear(s),
        }
    }
}

pub struct Judge {
    client: Client,
    cfg: TypeSafeConfig,
    policy: Policy,
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
        let policy = Policy::from(&cfg);
        Ok(Self {
            client,
            cfg,
            policy,
        })
    }

    /// The thresholds this judge applies, so the router can gate the fast
    /// path with the same ones.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub async fn judge_verdict(&self, utterance: &str, ctx: &Context<'_>) -> anyhow::Result<Verdict> {
        let candidates = Candidates::build(utterance, ctx, &self.cfg);
        let state = candidates.state(utterance, ctx);

        let t0 = std::time::Instant::now();
        let resp = self.client.evaluate(&state, &candidates.questions).await?;
        let (tokens_in, tokens_out) = resp
            .usage
            .as_ref()
            .map_or((0, 0), |u| (u.input_tokens, u.output_tokens));
        tracing::info!(
            "judged {:?} in {:.0}ms ({tokens_in} in / {tokens_out} out tokens)",
            utterance,
            t0.elapsed().as_secs_f64() * 1000.0,
        );

        Ok(compose(&resp, &self.policy, &candidates))
    }

    /// [`Self::judge_verdict`] with this judge's policy applied.
    pub async fn judge(&self, utterance: &str, ctx: &Context<'_>) -> anyhow::Result<Judgment> {
        Ok(self
            .judge_verdict(utterance, ctx)
            .await?
            .flatten(&self.policy))
    }
}

/// Turn the answers into an intent plus the signals that gate it. No
/// threshold is applied here except the dictation one, which decides what
/// kind of answer this is at all; the rest is [`Policy::decide`], so it can
/// be tested against recorded answers without a network call.
fn compose(resp: &Response, policy: &Policy, cand: &Candidates) -> Verdict {
    match build(resp, policy, cand) {
        Ok(v) => v,
        Err(reason) => Verdict::Unclear(reason),
    }
}

fn build(resp: &Response, policy: &Policy, cand: &Candidates) -> Result<Verdict, String> {
    let dictation = resp.noul("is_dictation");
    let destructive = resp.noul("is_destructive");
    if policy.is_prose(dictation) {
        return Ok(Verdict::Dictation);
    }

    let (name, intent_conf) = match cand.pick(resp, "intent")? {
        Pick::Chosen(n, c) => (n, c),
        Pick::NoMatch(_) => return Err("not a desktop command".into()),
        Pick::Missing => return Err("model returned no intent".into()),
    };
    // Weakest link, not a product: one wrong argument spoils the action,
    // so the action is only as trustworthy as its least certain part.
    let mut confidence = intent_conf;
    let mut args: BTreeMap<String, String> = BTreeMap::new();
    let mut window_id = None;
    // The intent name to build; show_app resolves to launch or focus.
    let mut build_name = name;

    fn required<'a>(
        cand: &Candidates,
        resp: &'a Response,
        name: &str,
        id: &str,
        confidence: &mut f64,
    ) -> Result<&'a str, String> {
        cand.arg(resp, id, confidence)?
            .ok_or_else(|| format!("{name} without a {id}"))
    }

    match name {
        "show_app" => {
            // "make this visible" splits into launch-vs-focus on an observed
            // fact, so code decides it rather than spending model
            // probability on the split.
            match cand.target(cand.arg(resp, "target", &mut confidence)?) {
                Target::Window(w) => {
                    args.insert("query".into(), w.title.clone());
                    window_id = Some(w.id.clone());
                    build_name = "focus_window";
                }
                Target::App(app) => {
                    args.insert("query".into(), app.to_string());
                    build_name = "launch_app";
                }
                Target::Focused => {
                    return Err("show_app pointed at the window that already has focus".into())
                }
                Target::Unnamed => return Err("show_app without a target".into()),
            }
        }
        "close_window" | "minimize_window" | "maximize_window" => {
            match cand.target(cand.arg(resp, "target", &mut confidence)?) {
                Target::Window(w) => {
                    args.insert("query".into(), w.title.clone());
                    window_id = Some(w.id.clone());
                }
                Target::App(app) => {
                    args.insert("query".into(), app.to_string());
                }
                // Only an explicit, confident choice of the focused window
                // means "no query"; silence is not that choice.
                Target::Focused => {}
                Target::Unnamed => return Err(format!("{name} without a target")),
            }
        }
        "krunner" => {
            let key = required(cand, resp, name, "payload", &mut confidence)?;
            args.insert("query".into(), cand.span(key)?.to_string());
        }
        "virtual_desktop" => {
            let key = required(cand, resp, name, "desktop_number", &mut confidence)?;
            let n: u32 = key
                .parse()
                .map_err(|_| format!("desktop number {key:?} is not a number"))?;
            if !(1..=cand.desktop_count).contains(&n) {
                return Err(format!(
                    "desktop {n} does not exist (this machine has {})",
                    cand.desktop_count
                ));
            }
            args.insert("n".into(), n.to_string());
        }
        "virtual_desktop_rel" => {
            let delta = match required(cand, resp, name, "desktop_direction", &mut confidence)? {
                "next" => "1",
                "previous" => "-1",
                other => return Err(format!("desktop direction {other:?} is not next or previous")),
            };
            args.insert("delta".into(), delta.into());
        }
        "start_claude" => {
            // Optional: absent leaves the configured default model.
            if let Some(m) = cand.arg(resp, "claude_model", &mut confidence)? {
                args.insert("model".into(), m.to_string());
            }
        }
        "claude_model" => {
            let m = required(cand, resp, name, "claude_model", &mut confidence)?;
            args.insert("model".into(), m.to_string());
        }
        "claude_tell" | "notify" => {
            let key = required(cand, resp, name, "payload", &mut confidence)?;
            args.insert("text".into(), cand.span(key)?.to_string());
        }
        "key" => {
            let key = required(cand, resp, name, "payload", &mut confidence)?;
            args.insert("chord".into(), cand.span(key)?.replace(" plus ", " "));
        }
        "open_terminal" | "claude_read" => {}
        other => return Err(format!("no rule to build intent {other:?}")),
    }

    let intent = Intent::from_args(build_name, &args)
        .ok_or_else(|| format!("could not build {build_name} from {args:?}"))?;

    Ok(Verdict::Act(Resolved {
        intent,
        window_id,
        confidence,
        signals: Signals::Judged {
            confidence,
            destructive,
            dictation,
        },
    }))
}

/// One choice answer, checked against what was offered.
enum Pick<'a> {
    Chosen(&'a str, f64),
    NoMatch(f64),
    Missing,
}

/// What a `target` key denotes.
enum Target<'a> {
    App(&'a str),
    Window(&'a Window),
    Focused,
    Unnamed,
}

/// Candidate values assembled by code, for the model to select among, and
/// the questions built from them so answers can be checked against exactly
/// what was sent.
struct Candidates {
    /// Installed application names, keyed `a:<index>`.
    apps: Vec<String>,
    /// Open windows, keyed `w:<index>`.
    windows: Vec<Window>,
    /// Trailing spans of the utterance, keyed `p:<index>`.
    spans: Vec<String>,
    desktop_count: u32,
    send_window_titles: bool,
    questions: BTreeMap<String, Question>,
}

fn indexed(key: &str, prefix: &str) -> Option<usize> {
    key.strip_prefix(prefix)?.parse().ok()
}

impl Candidates {
    fn build(utterance: &str, ctx: &Context<'_>, cfg: &TypeSafeConfig) -> Self {
        let mut seen = BTreeSet::new();
        let mut apps = Vec::new();
        for e in ctx.index.shortlist(utterance, APP_CANDIDATES) {
            if seen.insert(e.name.to_lowercase()) {
                apps.push((e.name.clone(), e.generic_name.clone()));
            }
        }

        let mut seen_ids = BTreeSet::new();
        let windows: Vec<Window> = ctx
            .windows
            .iter()
            .filter(|w| seen_ids.insert(w.id.clone()))
            .take(WINDOW_CANDIDATES)
            .cloned()
            .collect();

        // Every trailing span, so a payload can be selected verbatim instead
        // of regenerated. "tell claude to fix the test" -> "to fix the test".
        let words: Vec<&str> = utterance.split_whitespace().collect();
        let tail = &words[words.len().saturating_sub(PAYLOAD_SPANS)..];
        let spans: Vec<String> = (0..tail.len()).map(|i| tail[i..].join(" ")).collect();

        Self::assemble(
            apps,
            windows,
            spans,
            ctx.current_desktop,
            ctx.desktop_count,
            cfg.send_window_titles,
        )
    }

    fn assemble(
        apps: Vec<(String, Option<String>)>,
        windows: Vec<Window>,
        spans: Vec<String>,
        current_desktop: u32,
        desktop_count: u32,
        send_window_titles: bool,
    ) -> Self {
        let mut targets: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for (i, (name, generic)) in apps.iter().enumerate() {
            let desc = match generic {
                Some(g) => format!("installed application {name} ({g})"),
                None => format!("installed application {name}"),
            };
            targets.insert(format!("{APP_KEY}{i}"), json!(desc));
        }
        let mut per_class: BTreeMap<String, usize> = BTreeMap::new();
        for (i, w) in windows.iter().enumerate() {
            let nth = per_class.entry(w.class.to_lowercase()).or_default();
            *nth += 1;
            let desc = if send_window_titles {
                format!("open window of {}, titled {:?}", w.class, w.title)
            } else {
                format!("open window #{nth} of {}", w.class)
            };
            targets.insert(format!("{WINDOW_KEY}{i}"), json!(desc));
        }
        targets.insert(
            FOCUSED.into(),
            json!("the window that currently has focus, because the user named no target"),
        );
        targets.insert(
            NO_TARGET.into(),
            json!("the utterance names no application or window"),
        );

        let mut desktops: BTreeMap<String, serde_json::Value> = (1..=desktop_count
            .min(DESKTOP_CANDIDATES))
            .map(|n| (n.to_string(), json!(format!("virtual desktop number {n}"))))
            .collect();
        desktops.insert(
            NO_TARGET.into(),
            json!("no specific desktop number is named"),
        );

        let mut payload: BTreeMap<String, serde_json::Value> = spans
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("{SPAN_KEY}{i}"), json!(s)))
            .collect();
        payload.insert(
            NO_TARGET.into(),
            json!("`utterance` carries no such message"),
        );

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
                    "note": "Candidates are the applications installed on this machine and the windows currently open (the `w:` keys match `open_windows`). Choose the open window when the user implies something already running.",
                }),
                criteria: targets,
            },
        );
        q.insert(
            "desktop_number".into(),
            Question::Choice {
                instructions: json!(format!("Assuming `utterance` asks to switch to a specific numbered virtual desktop, which number? The user is currently on desktop {current_desktop}.")),
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
        q.insert(
            "payload".into(),
            Question::Choice {
                instructions: json!({
                    "task": "Assuming `utterance` carries a message to pass on verbatim — a prompt for Claude Code, a notification body, a search string, or a key chord — which candidate is exactly that message?",
                    "focus": "Each candidate's description is a trailing span of `utterance`. Pick the one that starts where the command wrapper ends, keeping the message itself complete and unaltered.",
                    "example": "For 'tell claude to fix the failing test', the message is 'to fix the failing test', not the whole utterance.",
                }),
                criteria: payload,
            },
        );

        debug_assert!(q
            .values()
            .filter_map(Question::options)
            .all(|o| o.len() <= MAX_CHOICE_OPTIONS));

        Self {
            apps: apps.into_iter().map(|(name, _)| name).collect(),
            windows,
            spans,
            desktop_count,
            send_window_titles,
            questions: q,
        }
    }

    /// The observed state the questions refer to. Window titles are included
    /// only when the config allows them off the machine.
    fn state(&self, utterance: &str, ctx: &Context<'_>) -> serde_json::Value {
        let open_windows: Vec<serde_json::Value> = self
            .windows
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let mut v = json!({
                    "key": format!("{WINDOW_KEY}{i}"),
                    "application": w.class,
                });
                if self.send_window_titles {
                    v["title"] = json!(w.title);
                }
                v
            })
            .collect();
        json!({
            "utterance": utterance,
            "open_windows": open_windows,
            "current_desktop": ctx.current_desktop,
            "desktop_count": ctx.desktop_count,
            "claude_code_session_running": ctx.claude_running,
        })
    }

    /// Was `key` among the options we sent for choice question `id`?
    fn offered(&self, id: &str, key: &str) -> bool {
        self.questions
            .get(id)
            .and_then(Question::options)
            .is_some_and(|o| o.contains_key(key))
    }

    /// Read one choice answer. A value we never offered is an error rather
    /// than a guess: the model cannot invent an option.
    fn pick<'a>(&self, resp: &'a Response, id: &str) -> Result<Pick<'a>, String> {
        let Some((key, conf)) = resp.choice(id) else {
            return Ok(Pick::Missing);
        };
        if !self.offered(id, key) {
            return Err(format!("model chose {key:?} for {id}, which was never offered"));
        }
        Ok(if key == NO_TARGET {
            Pick::NoMatch(conf)
        } else {
            Pick::Chosen(key, conf)
        })
    }

    /// Read one chosen argument key, folding its confidence into the running
    /// minimum. A no-match answer yields None but still folds in: a shaky
    /// "nothing named" is as much a doubt as a shaky name.
    fn arg<'a>(
        &self,
        resp: &'a Response,
        id: &str,
        confidence: &mut f64,
    ) -> Result<Option<&'a str>, String> {
        Ok(match self.pick(resp, id)? {
            Pick::Chosen(key, c) => {
                *confidence = confidence.min(c);
                Some(key)
            }
            Pick::NoMatch(c) => {
                *confidence = confidence.min(c);
                None
            }
            Pick::Missing => None,
        })
    }

    fn target(&self, key: Option<&str>) -> Target<'_> {
        let Some(key) = key else {
            return Target::Unnamed;
        };
        if key == FOCUSED {
            return Target::Focused;
        }
        if let Some(app) = indexed(key, APP_KEY).and_then(|i| self.apps.get(i)) {
            return Target::App(app);
        }
        if let Some(w) = indexed(key, WINDOW_KEY).and_then(|i| self.windows.get(i)) {
            return Target::Window(w);
        }
        Target::Unnamed
    }

    fn span(&self, key: &str) -> Result<&str, String> {
        indexed(key, SPAN_KEY)
            .and_then(|i| self.spans.get(i))
            .map(String::as_str)
            .ok_or_else(|| format!("payload key {key:?} names no span"))
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
mod tests {
    use super::*;

    /// Recorded answers, exactly as the API returns them.
    fn resp(json: serde_json::Value) -> Response {
        serde_json::from_value(serde_json::json!({
            "model": "jev-latest",
            "answers": json,
            "usage": { "input_tokens": 0, "output_tokens": 0 },
        }))
        .expect("recorded answer should deserialize")
    }

    fn choice(c: &str, conf: f64) -> serde_json::Value {
        serde_json::json!({
            "type": "choice",
            "choice": c,
            "confidence": conf,
            "probabilities": { c: conf },
        })
    }

    fn noul(p: f64) -> serde_json::Value {
        serde_json::json!({ "type": "noul", "noul": p })
    }

    fn window(id: &str, title: &str, class: &str) -> Window {
        Window {
            id: id.into(),
            title: title.into(),
            class: class.into(),
        }
    }

    const KONSOLE: (&str, &str, &str) = ("{k1}", "build — Konsole", "konsole");

    /// Apps keyed `a:<i>` in order, windows `w:<i>`, spans `p:<i>` from the
    /// utterance's words. Titles are sent, as on an opted-in machine.
    fn cand_with(apps: &[&str], windows: &[(&str, &str, &str)], desktops: u32, utterance: &str) -> Candidates {
        let words: Vec<&str> = utterance.split_whitespace().collect();
        Candidates::assemble(
            apps.iter().map(|a| ((*a).to_string(), None)).collect(),
            windows.iter().map(|(i, t, c)| window(i, t, c)).collect(),
            (0..words.len()).map(|i| words[i..].join(" ")).collect(),
            1,
            desktops,
            true,
        )
    }

    fn cand() -> Candidates {
        cand_with(&["Firefox", "Kate"], &[KONSOLE], 2, "")
    }

    fn policy() -> Policy {
        Policy::default()
    }

    /// The built intent plus what the default policy makes of it.
    fn act(v: Verdict) -> (Resolved, Decision) {
        match v {
            Verdict::Act(r) => {
                let d = policy().decide(&r.intent, &r.signals);
                (r, d)
            }
            other => panic!("expected Act, got {other:?}"),
        }
    }

    fn refused(v: Verdict) -> String {
        match act(v) {
            (_, Decision::Refuse { reason }) => reason,
            (r, d) => panic!("expected Refuse, got {d:?} for {r:?}"),
        }
    }

    fn unclear(v: Verdict) -> String {
        match v {
            Verdict::Unclear(s) => s,
            other => panic!("expected Unclear, got {other:?}"),
        }
    }

    #[test]
    fn confident_launch_acts_without_confirmation() {
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.94),
            "target": choice("a:0", 0.99),
            "is_dictation": noul(0.02),
            "is_destructive": noul(0.03),
        }));
        let (got, decision) = act(compose(&r, &policy(), &cand()));
        assert_eq!(
            got.intent,
            Intent::LaunchApp {
                query: "Firefox".into()
            }
        );
        assert_eq!(decision, Decision::Act);
        assert_eq!(got.window_id, None);
    }

    #[test]
    fn confidence_is_the_weakest_link_not_the_product() {
        // A certain intent with a shaky argument is only as good as the
        // argument; multiplying would have given 0.47 and read as unclear.
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.95),
            "target": choice("a:1", 0.50),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.01),
        }));
        let (got, decision) = act(compose(&r, &policy(), &cand()));
        assert_eq!(got.confidence, 0.50);
        assert!(matches!(decision, Decision::Confirm { .. }));
    }

    #[test]
    fn no_match_confidence_is_folded_in() {
        // A shaky "no model named" is a doubt about the whole action, so it
        // must drag the confidence down instead of vanishing.
        let r = resp(serde_json::json!({
            "intent": choice("start_claude", 0.99),
            "claude_model": choice(NO_TARGET, 0.50),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.02),
        }));
        let (got, decision) = act(compose(&r, &policy(), &cand()));
        assert_eq!(got.intent, Intent::StartClaude { model: None });
        assert_eq!(got.confidence, 0.50);
        assert!(matches!(decision, Decision::Confirm { .. }));

        let r = resp(serde_json::json!({
            "intent": choice("start_claude", 0.99),
            "claude_model": choice(NO_TARGET, 0.20),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.02),
        }));
        assert!(refused(compose(&r, &policy(), &cand())).contains("0.20"));
    }

    #[test]
    fn dictation_wins_over_any_intent() {
        // Prose that happens to read like a command must not be executed.
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.99),
            "target": choice("a:0", 0.99),
            "is_dictation": noul(0.88),
            "is_destructive": noul(0.01),
        }));
        assert!(matches!(
            compose(&r, &policy(), &cand()),
            Verdict::Dictation
        ));
    }

    #[test]
    fn judged_risk_forces_confirmation_even_when_certain() {
        // Understanding the request perfectly is not permission to carry it
        // out: closing a terminal mid-build still asks.
        let r = resp(serde_json::json!({
            "intent": choice("close_window", 1.0),
            "target": choice("w:0", 1.0),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.91),
        }));
        let (got, decision) = act(compose(&r, &policy(), &cand()));
        assert_eq!(
            got.intent,
            Intent::CloseWindow {
                query: Some("build — Konsole".into())
            }
        );
        assert_eq!(got.window_id.as_deref(), Some("{k1}"));
        assert!(matches!(decision, Decision::Confirm { .. }));
    }

    #[test]
    fn missing_safety_answers_force_confirmation() {
        // No is_destructive answer is not "safe": a harmless-looking
        // minimize must still ask.
        let r = resp(serde_json::json!({
            "intent": choice("minimize_window", 0.95),
            "target": choice(FOCUSED, 0.95),
            "is_dictation": noul(0.01),
        }));
        let (_, decision) = act(compose(&r, &policy(), &cand()));
        assert!(matches!(decision, Decision::Confirm { .. }));

        // Likewise a missing (or wrong-typed) is_dictation answer.
        let r = resp(serde_json::json!({
            "intent": choice("minimize_window", 0.95),
            "target": choice(FOCUSED, 0.95),
            "is_dictation": choice("yes", 0.9),
            "is_destructive": noul(0.01),
        }));
        let (_, decision) = act(compose(&r, &policy(), &cand()));
        assert!(matches!(decision, Decision::Confirm { .. }));
    }

    #[test]
    fn explicit_focused_window_judged_safe_skips_confirmation() {
        // The structural rule marks every close_window destructive; the
        // judged one lets minimizing the focused window through.
        let r = resp(serde_json::json!({
            "intent": choice("minimize_window", 0.92),
            "target": choice(FOCUSED, 0.95),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.04),
        }));
        let (got, decision) = act(compose(&r, &policy(), &cand()));
        assert_eq!(got.intent, Intent::MinimizeWindow { query: None });
        assert_eq!(decision, Decision::Act);
    }

    #[test]
    fn minimize_with_no_target_is_unclear() {
        // Neither "nothing named" nor silence means the focused window.
        let none = resp(serde_json::json!({
            "intent": choice("minimize_window", 0.92),
            "target": choice(NO_TARGET, 0.95),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.04),
        }));
        assert!(unclear(compose(&none, &policy(), &cand())).contains("without a target"));

        let missing = resp(serde_json::json!({
            "intent": choice("close_window", 0.92),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.04),
        }));
        assert!(unclear(compose(&missing, &policy(), &cand())).contains("without a target"));
    }

    #[test]
    fn show_app_on_the_focused_window_is_unclear() {
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.9),
            "target": choice(FOCUSED, 0.9),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.01),
        }));
        unclear(compose(&r, &policy(), &cand()));
    }

    #[test]
    fn no_match_and_low_confidence_both_refuse() {
        let none = resp(serde_json::json!({
            "intent": choice(NO_TARGET, 0.9),
            "is_dictation": noul(0.1),
        }));
        unclear(compose(&none, &policy(), &cand()));

        // Low confidence with an otherwise complete answer is the policy's
        // refusal, and the flattened view turns that into Unclear.
        let shaky = resp(serde_json::json!({
            "intent": choice("close_window", 0.20),
            "target": choice(FOCUSED, 0.9),
            "is_dictation": noul(0.1),
            "is_destructive": noul(0.1),
        }));
        refused(compose(&shaky, &policy(), &cand()));
        assert!(matches!(
            compose(&shaky, &policy(), &cand()).flatten(&policy()),
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
        unclear(compose(&r, &policy(), &cand()));
    }

    #[test]
    fn out_of_range_desktop_is_unclear() {
        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop", 0.97),
            "desktop_number": choice("3", 0.99),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.01),
        }));
        // Two desktops were offered, so "3" was never a candidate.
        assert!(unclear(compose(&r, &policy(), &cand())).contains("never offered"));

        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop", 0.97),
            "desktop_number": choice("2", 0.99),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.01),
        }));
        let (got, _) = act(compose(&r, &policy(), &cand()));
        assert_eq!(got.intent, Intent::VirtualDesktop { n: 2 });
    }

    #[test]
    fn unknown_direction_is_unclear_not_next() {
        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop_rel", 0.97),
            "desktop_direction": choice("sideways", 0.99),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.01),
        }));
        unclear(compose(&r, &policy(), &cand()));

        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop_rel", 0.97),
            "desktop_direction": choice("previous", 0.99),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.01),
        }));
        let (got, _) = act(compose(&r, &policy(), &cand()));
        assert_eq!(got.intent, Intent::VirtualDesktopRel { delta: -1 });
    }

    #[test]
    fn unoffered_choice_is_rejected() {
        // The model returning a bare app name instead of a key is a value we
        // never sent; it must not resolve to anything.
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.94),
            "target": choice("Firefox", 0.99),
            "is_dictation": noul(0.02),
            "is_destructive": noul(0.03),
        }));
        assert!(unclear(compose(&r, &policy(), &cand())).contains("never offered"));

        let r = resp(serde_json::json!({
            "intent": choice("reboot", 0.94),
            "is_dictation": noul(0.02),
            "is_destructive": noul(0.03),
        }));
        assert!(unclear(compose(&r, &policy(), &cand())).contains("never offered"));
    }

    #[test]
    fn optional_model_absent_leaves_the_default() {
        let r = resp(serde_json::json!({
            "intent": choice("start_claude", 0.96),
            "claude_model": choice(NO_TARGET, 0.98),
            "is_dictation": noul(0.01),
            "is_destructive": noul(0.02),
        }));
        let (got, decision) = act(compose(&r, &policy(), &cand()));
        assert_eq!(got.intent, Intent::StartClaude { model: None });
        assert_eq!(decision, Decision::Act);
    }

    #[test]
    fn payload_span_is_taken_verbatim_and_claude_tell_confirms() {
        let c = cand_with(&[], &[], 1, "tell claude to rerun the failing test");
        let r = resp(serde_json::json!({
            "intent": choice("claude_tell", 0.93),
            "payload": choice("p:2", 0.9),
            "is_dictation": noul(0.2),
            "is_destructive": noul(0.05),
        }));
        let (got, decision) = act(compose(&r, &policy(), &c));
        assert_eq!(
            got.intent,
            Intent::ClaudeTell {
                text: "to rerun the failing test".into()
            }
        );
        assert!(matches!(decision, Decision::Confirm { .. }));
    }

    #[test]
    fn krunner_searches_for_the_payload() {
        let c = cand_with(&["Firefox"], &[], 1, "search for quarterly report");
        let r = resp(serde_json::json!({
            "intent": choice("krunner", 0.93),
            "target": choice("a:0", 0.9),
            "payload": choice("p:2", 0.9),
            "is_dictation": noul(0.02),
            "is_destructive": noul(0.01),
        }));
        let (got, _) = act(compose(&r, &policy(), &c));
        assert_eq!(
            got.intent,
            Intent::KRunner {
                query: "quarterly report".into()
            }
        );
    }

    #[test]
    fn sentinel_titled_window_is_not_no_match() {
        let c = cand_with(&[], &[("{w9}", NO_TARGET, "kate"), ("{w10}", FOCUSED, "kate")], 1, "");
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.9),
            "target": choice("w:0", 0.95),
            "is_dictation": noul(0.02),
            "is_destructive": noul(0.02),
        }));
        let (got, _) = act(compose(&r, &policy(), &c));
        assert_eq!(
            got.intent,
            Intent::FocusWindow {
                query: NO_TARGET.into()
            }
        );
        assert_eq!(got.window_id.as_deref(), Some("{w9}"));

        let r = resp(serde_json::json!({
            "intent": choice("minimize_window", 0.9),
            "target": choice("w:1", 0.95),
            "is_dictation": noul(0.02),
            "is_destructive": noul(0.02),
        }));
        let (got, _) = act(compose(&r, &policy(), &c));
        assert_eq!(
            got.intent,
            Intent::MinimizeWindow {
                query: Some(FOCUSED.into())
            }
        );
        assert_eq!(got.window_id.as_deref(), Some("{w10}"));
    }

    #[test]
    fn show_app_becomes_launch_when_nothing_is_open() {
        let c = cand_with(&["Dolphin"], &[KONSOLE], 1, "");
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.9),
            "target": choice("a:0", 0.95),
            "is_dictation": noul(0.02),
            "is_destructive": noul(0.02),
        }));
        let (got, _) = act(compose(&r, &policy(), &c));
        assert_eq!(
            got.intent,
            Intent::LaunchApp {
                query: "Dolphin".into()
            }
        );
    }

    #[test]
    fn show_app_becomes_focus_when_the_window_exists() {
        // Same wish, different observed state: code flips the verb, and no
        // model probability was spent on the distinction.
        let c = cand_with(&["Dolphin"], &[KONSOLE], 1, "");
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.9),
            "target": choice("w:0", 0.95),
            "is_dictation": noul(0.02),
            "is_destructive": noul(0.02),
        }));
        let (got, _) = act(compose(&r, &policy(), &c));
        assert_eq!(
            got.intent,
            Intent::FocusWindow {
                query: "build — Konsole".into()
            }
        );
        assert_eq!(got.window_id.as_deref(), Some("{k1}"));
    }

    #[test]
    fn window_titles_stay_home_unless_opted_in() {
        let index = DesktopIndex::from_dirs(&[]);
        let windows = vec![window("{w1}", "secret-plan.md — Kate", "kate")];
        let ctx = Context {
            windows: &windows,
            index: &index,
            current_desktop: 1,
            desktop_count: 1,
            claude_running: false,
        };
        let sent = |send_window_titles: bool| {
            let cfg = TypeSafeConfig {
                send_window_titles,
                ..TypeSafeConfig::default()
            };
            let c = Candidates::build("bring up kate", &ctx, &cfg);
            let state = serde_json::to_string(&c.state("bring up kate", &ctx)).unwrap();
            let questions = serde_json::to_string(&c.questions).unwrap();
            state + &questions
        };
        let private = sent(false);
        assert!(!private.contains("secret-plan"), "{private}");
        assert!(private.contains("kate"));
        assert!(sent(true).contains("secret-plan.md"));
    }

    #[test]
    fn windows_dedupe_by_id_and_questions_stay_within_the_option_limit() {
        let index = DesktopIndex::from_dirs(&[]);
        let mut windows: Vec<Window> = (0..300)
            .map(|i| window(&format!("{{w{i}}}"), "Same title", "kate"))
            .collect();
        windows.push(window("{w0}", "Same title", "kate"));
        let ctx = Context {
            windows: &windows,
            index: &index,
            current_desktop: 1,
            desktop_count: 400,
            claude_running: false,
        };
        let utterance = vec!["word"; 300].join(" ");
        let c = Candidates::build(&utterance, &ctx, &TypeSafeConfig::default());
        assert_eq!(c.windows.len(), WINDOW_CANDIDATES);
        assert_eq!(c.spans.len(), PAYLOAD_SPANS);
        for (id, q) in &c.questions {
            if let Some(o) = q.options() {
                assert!(o.len() <= MAX_CHOICE_OPTIONS, "{id} offers {}", o.len());
            }
        }
        let n = c.questions["desktop_number"].options().unwrap().len();
        assert_eq!(n, MAX_CHOICE_OPTIONS);
    }
}
