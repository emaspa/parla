//! The judged path: what to do with an utterance the grammar rejected.
//!
//! `Grammar::parse` matches literal word sequences, so it answers in
//! microseconds and misses anything phrased differently ("bring up firefox",
//! "kill that window"). Rather than hand those to a conversational agent, this
//! asks the model which intent was meant (and whether anything was meant at
//! all), then only the questions that intent still needs: a target, a
//! desktop, a model name, or a message when no cue rule found it. Which
//! model answers is the [`Oracle`]'s business: a GGUF on this machine's GPU
//! by default, or the TypeSafe API.
//!
//! Code still owns everything code is good at: which apps are installed, which
//! windows are open, how many desktops exist, what a "remind me to" or "hit
//! control s" carries, which intents can destroy work, and what counts as
//! confident enough to act. The model only supplies the semantic step — which
//! of the candidates the user meant. Every candidate is offered under an
//! opaque key (`a:3`, `w:0`, `p:2`) with the human-readable value in its
//! description, so a window titled `__none__` can never be mistaken for the
//! no-match option, and every returned choice is checked against what was
//! offered.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use desktopd::DesktopIndex;
use parla_grammar::chord::spoken_chord;
use parla_grammar::Intent;
use serde_json::json;

use crate::config::{Backend, JudgeConfig};
use crate::local::LocalModel;
use crate::oracle::{
    Answer, Criteria, NoulCriteria, Oracle, Question, Response, MAX_CHOICE_OPTIONS,
};
use crate::payload::strip_cue;
use crate::policy::{risk_rule, Policy, Risk, Signals};
use crate::typesafe::Client;
use desktopd::windows::Window;

/// Sentinel option names. Real candidates use `a:`/`w:`/`p:` keys, so these
/// cannot collide with anything observed.
const NO_TARGET: &str = "__none__";
const FOCUSED: &str = "__focused_window__";
/// The whole utterance as a payload, offered last: the answer only when no
/// part of it is a command wrapper.
const WHOLE: &str = "__whole__";
/// The three readings of an utterance the dictation question offers.
const DICTATION_COMMAND: &str = "command";
const DICTATION_TEXT: &str = "text";
const DICTATION_NEITHER: &str = "neither";

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
    /// the policy and the logs; execution goes by this id.
    pub window_id: Option<String>,
    /// `.desktop` id of the application the user named, when the target was
    /// chosen from the installed-application list.
    pub entry_id: Option<String>,
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

/// A verdict together with the raw signals it was built from, for
/// `parlad --calibrate`. The verdict here is composed without the
/// dictation gate: a case the model calls prose still shows which intent
/// it would have built, and the caller applies whichever dictation
/// threshold it is measuring to `dictation` itself.
#[derive(Debug)]
pub struct Detailed {
    pub verdict: Verdict,
    /// Probability of yes to `is_dictation`, before any threshold.
    pub dictation: Option<f64>,
    /// Probability of yes to `is_destructive`, before any threshold; None
    /// when the intent's risk was decided by rule and the model not asked.
    pub destructive: Option<f64>,
    /// The intent key the model ranked first and its probability, whatever
    /// became of it; the no-match sentinel when it chose that.
    pub intent: Option<(String, f64)>,
}

pub struct Judge {
    oracle: Oracle,
    policy: Policy,
    /// Whether window titles go into the questions. Always for a local
    /// model; for the API only when the config allows them off the machine.
    send_window_titles: bool,
}

/// Context code gathers before asking. Everything here is observed fact, kept
/// separate from anything the model infers.
pub struct Context<'a> {
    pub windows: &'a [Window],
    pub index: &'a DesktopIndex,
    pub current_desktop: u32,
    pub desktop_count: u32,
    pub claude_running: bool,
    /// Something was dictated a moment ago, so "make that shorter" has a
    /// referent and `edit_text` is on offer.
    pub last_dictation: bool,
}

impl Judge {
    /// Build the backend the config names. `local` is the already-loaded
    /// model when the config asks for one.
    pub fn new(cfg: &JudgeConfig, local: Option<Arc<LocalModel>>) -> anyhow::Result<Self> {
        let timeout = std::time::Duration::from_millis(cfg.timeout_ms);
        let oracle = match cfg.backend {
            Backend::Local => Oracle::Local(
                local.ok_or_else(|| anyhow::anyhow!("judge.backend = \"local\" but no local model"))?,
                timeout,
            ),
            Backend::TypeSafe => Oracle::TypeSafe(Client::new(
                cfg.typesafe.resolved_api_key()?,
                cfg.typesafe.model.clone(),
                timeout,
            )?),
        };
        let send_window_titles = oracle.is_local() || cfg.send_window_titles;
        Ok(Self {
            oracle,
            policy: Policy::from(cfg),
            send_window_titles,
        })
    }

    /// The backend and model, for a log line.
    pub fn describe(&self) -> String {
        self.oracle.describe()
    }

    /// The thresholds this judge applies, so the router can gate the fast
    /// path with the same ones.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub async fn judge_verdict(&self, utterance: &str, ctx: &Context<'_>) -> anyhow::Result<Verdict> {
        let d = self.judge_verdict_detailed(utterance, ctx).await?;
        Ok(if self.policy.is_prose(d.dictation) {
            Verdict::Dictation
        } else {
            d.verdict
        })
    }

    /// [`Self::judge_verdict`] plus the probabilities behind it; see
    /// [`Detailed`] for what is and is not applied.
    pub async fn judge_verdict_detailed(
        &self,
        utterance: &str,
        ctx: &Context<'_>,
    ) -> anyhow::Result<Detailed> {
        let candidates = Candidates::build(utterance, ctx, self.send_window_titles);
        let state = candidates.state(utterance, ctx);

        // Two rounds: the intent (and whether this was prose at all) first,
        // then only the questions that intent needs. A message the cue
        // rules already found and a risk the intent itself decides are
        // never asked. The state is shared through the cache locally, so
        // the second round costs its own questions and nothing more.
        let t0 = std::time::Instant::now();
        let mut resp = self.oracle.evaluate(&state, &candidates.first_round()).await?;
        if let Some((intent, _)) = resp.choice("intent") {
            let second = candidates.second_round(intent);
            if !second.is_empty() {
                let more = self.oracle.evaluate(&state, &second).await?;
                resp.answers.extend(more.answers);
                if let (Some(u), Some(m)) = (resp.usage.as_mut(), more.usage) {
                    u.input_tokens += m.input_tokens;
                    u.output_tokens += m.output_tokens;
                }
            }
        }
        let (tokens_in, tokens_out) = resp
            .usage
            .as_ref()
            .map_or((0, 0), |u| (u.input_tokens, u.output_tokens));
        tracing::info!(
            "judged {:?} in {:.0}ms ({tokens_in} in / {tokens_out} out tokens)",
            utterance,
            t0.elapsed().as_secs_f64() * 1000.0,
        );
        for (id, answer) in &resp.answers {
            tracing::debug!("answer {id}: {}", candidates.describe(answer));
        }

        Ok(Detailed {
            verdict: compose(&resp, &candidates),
            dictation: dictation_probability(&resp),
            destructive: resp.noul("is_destructive"),
            intent: resp.choice("intent").map(|(k, p)| (k.to_string(), p)),
        })
    }
}

/// Turn the answers into an intent plus the signals that gate it. No
/// threshold is applied here: whether the utterance was prose, and whether
/// the intent runs, asks or is refused, is [`Policy::decide`], so this can
/// be tested against recorded answers without a network call.
fn compose(resp: &Response, cand: &Candidates) -> Verdict {
    match build(resp, cand) {
        Ok(v) => v,
        Err(reason) => Verdict::Unclear(reason),
    }
}

/// The probability that the utterance was text to type: the weight the
/// dictation question put on its "text" reading. None when the question
/// was not answered as a choice, or the reading was not among the options
/// answered, so the policy asks rather than assumes.
fn dictation_probability(resp: &Response) -> Option<f64> {
    match resp.answers.get("is_dictation") {
        Some(Answer::Choice { probabilities, .. }) => probabilities.get(DICTATION_TEXT).copied(),
        _ => None,
    }
}

fn build(resp: &Response, cand: &Candidates) -> Result<Verdict, String> {
    let dictation = dictation_probability(resp);

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
    let mut entry_id = None;
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

    // The message an intent passes on: by rule when a cue phrase starts
    // the utterance, otherwise the span the model picked. A rule's answer
    // was never in doubt, so it leaves the confidence alone.
    let payload = |resp: &Response, confidence: &mut f64| -> Result<String, String> {
        match strip_cue(name, &cand.utterance) {
            Some(text) => Ok(text),
            None => {
                let key = required(cand, resp, name, "payload", confidence)?;
                Ok(cand.span(key)?.to_string())
            }
        }
    };

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
                    args.insert("query".into(), app.name.clone());
                    entry_id = Some(app.id.clone());
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
                    args.insert("query".into(), app.name.clone());
                }
                // Only an explicit, confident choice of the focused window
                // means "no query"; silence is not that choice.
                Target::Focused => {}
                Target::Unnamed => return Err(format!("{name} without a target")),
            }
        }
        "krunner" => {
            args.insert("query".into(), payload(resp, &mut confidence)?);
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
            args.insert("text".into(), payload(resp, &mut confidence)?);
        }
        "key" => {
            // Spoken words become key names, or the intent is unclear: a
            // chord is injected into whatever has focus, so a word that is
            // not a key must not become a guess.
            let spoken = payload(resp, &mut confidence)?;
            let chord = spoken_chord(&spoken)
                .ok_or_else(|| format!("{spoken:?} is not a key chord"))?;
            args.insert("chord".into(), chord);
        }
        "edit_text" => {
            // The whole utterance is the instruction; the model that
            // applies it reads it as spoken.
            args.insert("instruction".into(), cand.utterance.clone());
        }
        "open_terminal" | "claude_read" => {}
        other => return Err(format!("no rule to build intent {other:?}")),
    }

    let intent = Intent::from_args(build_name, &args)
        .ok_or_else(|| format!("could not build {build_name} from {args:?}"))?;

    // Risk is the intent's own where the intent decides it. Only an
    // intent the rule leaves to the model carries the model's answer, and
    // for one that always confirms the policy asks before it would look.
    let destructive = match risk_rule(name) {
        Risk::Never => Some(0.0),
        Risk::AlwaysConfirmed => None,
        Risk::Judged => resp.noul("is_destructive"),
    };

    Ok(Verdict::Act(Resolved {
        intent,
        window_id,
        entry_id,
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
    App(&'a AppCandidate),
    Window(&'a Window),
    Focused,
    Unnamed,
}

/// One installed application offered to the model.
#[derive(Debug, Clone)]
struct AppCandidate {
    /// `.desktop` id, so a chosen app launches by id rather than by a
    /// second fuzzy match on its name.
    id: String,
    name: String,
    generic_name: Option<String>,
}

/// Candidate values assembled by code, for the model to select among, and
/// the questions built from them so answers can be checked against exactly
/// what was sent.
struct Candidates {
    /// As spoken, for the intents that take the whole utterance.
    utterance: String,
    /// Installed applications, keyed `a:<index>`.
    apps: Vec<AppCandidate>,
    /// Open windows, keyed `w:<index>`.
    windows: Vec<Window>,
    /// Trailing spans of the utterance, keyed `p:<index>` by the word
    /// they start at. Index 0, the whole utterance, is offered as
    /// [`WHOLE`] instead, last.
    spans: Vec<String>,
    desktop_count: u32,
    send_window_titles: bool,
    /// Every question that may be asked, so an answer can be checked
    /// against what was offered whichever round asked it.
    questions: BTreeMap<String, Question>,
}

fn indexed(key: &str, prefix: &str) -> Option<usize> {
    key.strip_prefix(prefix)?.parse().ok()
}

impl Candidates {
    fn build(utterance: &str, ctx: &Context<'_>, send_window_titles: bool) -> Self {
        let mut seen = BTreeSet::new();
        let mut apps = Vec::new();
        for e in ctx.index.shortlist(utterance, APP_CANDIDATES) {
            if seen.insert(e.name.to_lowercase()) {
                apps.push(AppCandidate {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    generic_name: e.generic_name.clone(),
                });
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
        // The payload starts a few words in ("tell claude to ..."), so when
        // the utterance is long the start positions offered are the first
        // PAYLOAD_SPANS, each span running to the end of the utterance.
        let words: Vec<&str> = utterance.split_whitespace().collect();
        let starts = words.len().min(PAYLOAD_SPANS);
        let spans: Vec<String> = (0..starts).map(|i| words[i..].join(" ")).collect();

        Self::assemble(
            utterance,
            apps,
            windows,
            spans,
            ctx.current_desktop,
            ctx.desktop_count,
            send_window_titles,
            ctx.last_dictation,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        utterance: &str,
        apps: Vec<AppCandidate>,
        windows: Vec<Window>,
        spans: Vec<String>,
        current_desktop: u32,
        desktop_count: u32,
        send_window_titles: bool,
        last_dictation: bool,
    ) -> Self {
        // The candidates, then the two ways of naming none of them: a
        // "none of these" reads as one only after the list it rejects.
        let mut targets = Criteria::new();
        for (i, app) in apps.iter().enumerate() {
            let desc = match &app.generic_name {
                Some(g) => format!("installed application {} ({g})", app.name),
                None => format!("installed application {}", app.name),
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
            FOCUSED,
            json!("the window the user is working in right now: what `this window`, `the current window` or a request that names no window means"),
        );
        targets.insert(
            NO_TARGET,
            json!("none of these: the utterance names no application or window on this list"),
        );

        let mut desktops = Criteria::new();
        for n in 1..=desktop_count.min(DESKTOP_CANDIDATES) {
            desktops.insert(n.to_string(), json!(format!("virtual desktop number {n}")));
        }
        desktops.insert(NO_TARGET, json!("none of these: no desktop number is named, or the number named does not exist"));

        // Spans from the second word on: the first word of a message that
        // needed the model is a verb ("tell", "remind"), never the message.
        // The whole utterance is the last resort and reads as one.
        let mut payload = Criteria::new();
        for (i, s) in spans.iter().enumerate().skip(1) {
            payload.insert(format!("{SPAN_KEY}{i}"), json!(s));
        }
        payload.insert(NO_TARGET, json!("`utterance` carries no such message"));
        payload.insert(
            WHOLE,
            json!(format!(
                "all of `utterance`, {:?}: only if no word of it is a command wrapper",
                spans.first().map(String::as_str).unwrap_or("")
            )),
        );

        let intents = intent_criteria(last_dictation);
        let mut q = BTreeMap::new();
        q.insert(
            "intent".into(),
            Question::Choice {
                instructions: json!({
                    "task": "The user spoke `utterance` to a voice assistant that drives a KDE Plasma desktop. Decide which single action they asked for.",
                    "note": "Use `open_windows` to tell a request to start something new from a request to switch to something already running.",
                }),
                criteria: intents,
            },
        );
        // Three ways, not two: prose against "a command" alone read every
        // command that carries text (a reminder, a message for Claude Code)
        // as text. Named beside a question or noise, prose is what it is.
        q.insert(
            "is_dictation".into(),
            Question::Choice {
                instructions: json!("What was `utterance`: a command for this desktop assistant, text the user meant to have typed as spoken, or neither?"),
                criteria: Criteria::from([
                    (
                        DICTATION_COMMAND,
                        json!("asks the assistant to do something on this desktop or in Claude Code, even when it carries words to pass on: a notification's text, a search, a message for Claude Code, a change to the text just dictated"),
                    ),
                    (
                        DICTATION_TEXT,
                        json!("a sentence for a document, email, chat or note, to be typed as spoken: a statement, a greeting, a remark or a request to another person; it asks nothing of this desktop"),
                    ),
                    (
                        DICTATION_NEITHER,
                        json!("a question for the assistant to answer, a fragment, or noise"),
                    ),
                ]),
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
                    "note": "`a:` keys are applications installed on this machine, named with what they do; `w:` keys are the windows currently open, matching `open_windows`. An application asked for by name or by what it does is a match even if no window of it is open. Choose the open window when the user implies something already running.",
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
                criteria: Criteria::from([
                    ("next", json!("forward, to a higher-numbered desktop: the next or following one, to the right")),
                    ("previous", json!("back, to a lower-numbered desktop: the previous one, to the left")),
                    (NO_TARGET, json!("neither: no relative movement is asked for")),
                ]),
            },
        );
        q.insert(
            "claude_model".into(),
            Question::Choice {
                instructions: json!("Assuming `utterance` names which Claude model to use, which one? Choose the no-name option if the user does not mention a model at all, so the configured default stands."),
                criteria: Criteria::from([
                    ("opus", json!("the most capable model; also 'the big one', 'the smart one'")),
                    ("sonnet", json!("the balanced default")),
                    ("haiku", json!("the smallest and fastest; also 'the quick one', 'the small one', 'the cheap one'")),
                    (NO_TARGET, json!("none of these: no model is named or implied")),
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
            utterance: utterance.to_string(),
            apps,
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
            "text_dictated_moments_ago": ctx.last_dictation,
        })
    }

    /// One answer as a log line: the chosen key with the human-readable
    /// candidate it stands for, and the runner-up, so a threshold can be
    /// judged against what the model actually weighed.
    fn describe(&self, answer: &Answer) -> String {
        match answer {
            Answer::Noul { noul } => format!("{noul:.2}"),
            Answer::Choice {
                choice,
                confidence,
                probabilities,
            } => {
                let mut ranked: Vec<(&String, &f64)> = probabilities.iter().collect();
                ranked.sort_by(|a, b| b.1.total_cmp(a.1));
                let runner_up = ranked
                    .iter()
                    .find(|(k, _)| *k != choice)
                    .map(|(k, p)| format!(", then {} {p:.2}", self.name_of(k)))
                    .unwrap_or_default();
                format!("{} {confidence:.2}{runner_up}", self.name_of(choice))
            }
            Answer::Score { score, confidence } => format!("{score:.2} ({confidence:.2})"),
            Answer::Unknown => "unknown".into(),
        }
    }

    /// The key plus what it denotes, for logs.
    fn name_of(&self, key: &str) -> String {
        match self.target(Some(key)) {
            Target::App(app) => format!("{key} ({})", app.name),
            Target::Window(w) => format!("{key} ({})", w.title),
            _ => match self.span(key) {
                Ok(s) => format!("{key} ({s:?})"),
                Err(_) => key.to_string(),
            },
        }
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
        if key == WHOLE {
            return Ok(&self.utterance);
        }
        indexed(key, SPAN_KEY)
            .and_then(|i| self.spans.get(i))
            .map(String::as_str)
            .ok_or_else(|| format!("payload key {key:?} names no span"))
    }

    /// The questions every utterance gets: what was asked for, and whether
    /// anything was asked for at all.
    fn first_round(&self) -> BTreeMap<String, Question> {
        self.round(["intent", "is_dictation"])
    }

    /// The questions the chosen intent still needs. A message a cue rule
    /// found, and a risk the intent decides, are not among them. Empty for
    /// an intent that needs nothing more, and for no intent.
    fn second_round(&self, intent: &str) -> BTreeMap<String, Question> {
        if intent == NO_TARGET {
            return BTreeMap::new();
        }
        let mut ids: Vec<&str> = Vec::new();
        match intent {
            "show_app" | "close_window" | "minimize_window" | "maximize_window" => {
                ids.push("target");
            }
            "virtual_desktop" => ids.push("desktop_number"),
            "virtual_desktop_rel" => ids.push("desktop_direction"),
            "start_claude" | "claude_model" => ids.push("claude_model"),
            "krunner" | "notify" | "claude_tell" | "key"
                if strip_cue(intent, &self.utterance).is_none() =>
            {
                ids.push("payload");
            }
            _ => {}
        }
        if risk_rule(intent) == Risk::Judged {
            ids.push("is_destructive");
        }
        let mut q = self.round(ids);
        if let Some(Question::Choice { instructions, .. }) = q.get_mut("payload") {
            *instructions = payload_instructions(intent);
        }
        q
    }

    fn round<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, Question> {
        ids.into_iter()
            .filter_map(|id| Some((id.to_string(), self.questions.get(id)?.clone())))
            .collect()
    }
}

/// The payload question worded for the intent it serves, since by the
/// second round the intent is known.
fn payload_instructions(intent: &str) -> serde_json::Value {
    let (what, example) = match intent {
        "notify" => (
            "the text of a desktop notification the user asked to be shown",
            "For 'pop up a note saying lunch is ready', the message is 'lunch is ready'.",
        ),
        "krunner" => (
            "the search terms the user asked to be looked up",
            "For 'can you look for the vacation photos', the message is 'the vacation photos'.",
        ),
        "key" => (
            "the key chord the user asked to be pressed",
            "For 'hit control s', the message is 'control s'.",
        ),
        _ => (
            "the instruction the user wants passed to Claude Code as spoken",
            "For 'let claude know the tests pass', the message is 'the tests pass'.",
        ),
    };
    json!({
        "task": format!("`utterance` carries a message to pass on verbatim: {what}. Which candidate is exactly that message?"),
        "focus": "Each candidate is a trailing span of `utterance`. Pick the one that starts right after the words that ask for the action, keeping the message itself complete. The whole utterance is the answer only when it contains no such words.",
        "example": example,
    })
}

/// The intents on offer, in the order the model reads them: the refusal
/// first, then the actions by name, `edit_text` among them only while a
/// dictation is fresh. The order is deliberate and measured
/// (docs/commands.md): with the refusal last, as the target question has
/// it, three more corpus cases went to a wrong intent and one more wrong
/// act went unconfirmed.
fn intent_criteria(last_dictation: bool) -> Criteria {
    let mut intents = Criteria::new();
    intents.insert(
        NO_TARGET,
        json!("Not a desktop command, or too ambiguous to act on safely"),
    );
    let actions = [
        // Launch-vs-focus is deliberately absent: whether the target already
        // runs is an observed fact, so code decides it and the model's
        // probability is not split between two spellings of one wish.
        (
            "claude_model",
            json!("Change which model the running Claude Code session uses"),
        ),
        (
            "claude_read",
            json!("Read back what the Claude Code session has output so far, or report what it is doing now"),
        ),
        (
            "claude_tell",
            json!("Send the running Claude Code session an instruction, question or message to act on: have it fix, explain, add or run something"),
        ),
        (
            "close_window",
            json!("Close or quit a window"),
        ),
        (
            "edit_text",
            json!({
                "what": "Change the text the user dictated a moment ago: rewrite, shorten, expand, reformat, translate, fix, or change its tone",
                "examples": [
                    "make that more formal", "shorter", "turn that into bullet points",
                    "translate that to Italian", "capitalise the first word",
                ],
            }),
        ),
        (
            "key",
            json!("Press a key or key chord in the focused window: enter, escape, tab, control s, alt f4"),
        ),
        (
            "krunner",
            json!("Search this computer for a file, folder or document by name: find, look up, locate, where is. Not for starting a program"),
        ),
        (
            "maximize_window",
            json!("Maximize or full-screen a window"),
        ),
        (
            "minimize_window",
            json!("Hide a window from the screen without closing it: minimize, tuck away, send to the taskbar"),
        ),
        (
            "notify",
            json!("Show the user a desktop notification with the text they give"),
        ),
        (
            "open_terminal",
            json!("Open a new terminal window: a shell, a console"),
        ),
        (
            "show_app",
            json!({
                "what": "Put an application or window in front of the user, whether or not it is already running. The application may be named by what it does: the file manager, the calculator, the screenshot tool",
                "examples": [
                    "open firefox", "bring up my editor", "run the calculator",
                    "switch to the browser", "I need a terminal window",
                ],
            }),
        ),
        (
            "start_claude",
            json!("Start a new Claude Code coding session, optionally naming its model"),
        ),
        (
            "virtual_desktop",
            json!("Switch to a virtual desktop identified by number"),
        ),
        (
            "virtual_desktop_rel",
            json!("Move one virtual desktop forward or back from the current one"),
        ),
    ];
    for (name, desc) in actions {
        if name != "edit_text" || last_dictation {
            intents.insert(name, desc);
        }
    }
    intents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Decision;

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

    /// The dictation question answered with `p` on its "text" reading.
    fn dictation(p: f64) -> serde_json::Value {
        let (choice, conf) = if p >= 0.5 {
            (DICTATION_TEXT, p)
        } else {
            (DICTATION_COMMAND, 1.0 - p)
        };
        serde_json::json!({
            "type": "choice",
            "choice": choice,
            "confidence": conf,
            "probabilities": { DICTATION_TEXT: p, DICTATION_COMMAND: 1.0 - p },
        })
    }

    fn window(id: &str, title: &str, class: &str) -> Window {
        Window {
            id: id.into(),
            title: title.into(),
            class: class.into(),
            resource_name: String::new(),
            desktop: 1,
            active: false,
            minimized: false,
            stacking: 0,
            pid: 0,
            normal: true,
        }
    }

    const KONSOLE: (&str, &str, &str) = ("{k1}", "build — Konsole", "konsole");

    /// Apps keyed `a:<i>` in order, windows `w:<i>`, spans `p:<i>` from the
    /// utterance's words. Titles are sent, as on an opted-in machine.
    fn cand_with(apps: &[&str], windows: &[(&str, &str, &str)], desktops: u32, utterance: &str) -> Candidates {
        let words: Vec<&str> = utterance.split_whitespace().collect();
        Candidates::assemble(
            utterance,
            apps.iter()
                .map(|a| AppCandidate {
                    id: format!("{}.desktop", a.to_lowercase()),
                    name: (*a).to_string(),
                    generic_name: None,
                })
                .collect(),
            windows.iter().map(|(i, t, c)| window(i, t, c)).collect(),
            (0..words.len()).map(|i| words[i..].join(" ")).collect(),
            1,
            desktops,
            true,
            false,
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
            "is_dictation": dictation(0.02),
            "is_destructive": noul(0.03),
        }));
        let (got, decision) = act(compose(&r, &cand()));
        assert_eq!(
            got.intent,
            Intent::LaunchApp {
                query: "Firefox".into()
            }
        );
        assert_eq!(decision, Decision::Act);
        assert_eq!(got.window_id, None);
        assert_eq!(got.entry_id.as_deref(), Some("firefox.desktop"));
    }

    #[test]
    fn confidence_is_the_weakest_link_not_the_product() {
        // A certain intent with a shaky argument is only as good as the
        // argument; multiplying would have given 0.47 and read as unclear.
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.95),
            "target": choice("a:1", 0.50),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.01),
        }));
        let (got, decision) = act(compose(&r, &cand()));
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
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.02),
        }));
        let (got, decision) = act(compose(&r, &cand()));
        assert_eq!(got.intent, Intent::StartClaude { model: None });
        assert_eq!(got.confidence, 0.50);
        assert!(matches!(decision, Decision::Confirm { .. }));

        let r = resp(serde_json::json!({
            "intent": choice("start_claude", 0.99),
            "claude_model": choice(NO_TARGET, 0.20),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.02),
        }));
        assert!(refused(compose(&r, &cand())).contains("0.20"));
    }

    #[test]
    fn dictation_wins_over_any_intent() {
        // Prose that happens to read like a command must not be executed.
        // Composition keeps the intent (the calibrator wants to see it);
        // the policy refuses it, and `judge_verdict` reports Dictation.
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.99),
            "target": choice("a:0", 0.99),
            "is_dictation": dictation(0.88),
            "is_destructive": noul(0.01),
        }));
        let (got, decision) = act(compose(&r, &cand()));
        assert!(policy().is_prose(Some(0.88)));
        assert!(matches!(decision, Decision::Refuse { .. }), "{got:?}");
    }

    #[test]
    fn judged_risk_forces_confirmation_even_when_certain() {
        // Understanding the request perfectly is not permission to carry it
        // out: closing a terminal mid-build still asks.
        let r = resp(serde_json::json!({
            "intent": choice("close_window", 1.0),
            "target": choice("w:0", 1.0),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.91),
        }));
        let (got, decision) = act(compose(&r, &cand()));
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
        // No is_destructive answer is not "safe" for an intent whose risk
        // is the model's to judge: a search with no answer must still ask.
        let c = cand_with(&[], &[], 1, "look for the invoice");
        let r = resp(serde_json::json!({
            "intent": choice("krunner", 0.95),
            "is_dictation": dictation(0.01),
        }));
        let (_, decision) = act(compose(&r, &c));
        assert!(matches!(decision, Decision::Confirm { .. }));

        // Likewise a missing (or wrong-typed) is_dictation answer.
        let r = resp(serde_json::json!({
            "intent": choice("minimize_window", 0.95),
            "target": choice(FOCUSED, 0.95),
            "is_dictation": noul(0.1),
            "is_destructive": noul(0.01),
        }));
        let (_, decision) = act(compose(&r, &cand()));
        assert!(matches!(decision, Decision::Confirm { .. }));
    }

    #[test]
    fn explicit_focused_window_judged_safe_skips_confirmation() {
        // The structural rule marks every close_window destructive; the
        // judged one lets minimizing the focused window through.
        let r = resp(serde_json::json!({
            "intent": choice("minimize_window", 0.92),
            "target": choice(FOCUSED, 0.95),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.04),
        }));
        let (got, decision) = act(compose(&r, &cand()));
        assert_eq!(got.intent, Intent::MinimizeWindow { query: None });
        assert_eq!(decision, Decision::Act);
    }

    #[test]
    fn minimize_with_no_target_is_unclear() {
        // Neither "nothing named" nor silence means the focused window.
        let none = resp(serde_json::json!({
            "intent": choice("minimize_window", 0.92),
            "target": choice(NO_TARGET, 0.95),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.04),
        }));
        assert!(unclear(compose(&none, &cand())).contains("without a target"));

        let missing = resp(serde_json::json!({
            "intent": choice("close_window", 0.92),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.04),
        }));
        assert!(unclear(compose(&missing, &cand())).contains("without a target"));
    }

    #[test]
    fn show_app_on_the_focused_window_is_unclear() {
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.9),
            "target": choice(FOCUSED, 0.9),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.01),
        }));
        unclear(compose(&r, &cand()));
    }

    #[test]
    fn no_match_and_low_confidence_both_refuse() {
        let none = resp(serde_json::json!({
            "intent": choice(NO_TARGET, 0.9),
            "is_dictation": dictation(0.1),
        }));
        unclear(compose(&none, &cand()));

        // Low confidence with an otherwise complete answer is the policy's
        // refusal.
        let shaky = resp(serde_json::json!({
            "intent": choice("close_window", 0.20),
            "target": choice(FOCUSED, 0.9),
            "is_dictation": dictation(0.1),
            "is_destructive": noul(0.1),
        }));
        refused(compose(&shaky, &cand()));
    }

    #[test]
    fn missing_required_argument_refuses_rather_than_guessing() {
        // No desktop number was offered that fit, so there is nothing to act
        // on — better than switching to a desktop the user did not name.
        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop", 0.97),
            "desktop_number": choice(NO_TARGET, 0.99),
            "is_dictation": dictation(0.01),
        }));
        unclear(compose(&r, &cand()));
    }

    #[test]
    fn out_of_range_desktop_is_unclear() {
        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop", 0.97),
            "desktop_number": choice("3", 0.99),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.01),
        }));
        // Two desktops were offered, so "3" was never a candidate.
        assert!(unclear(compose(&r, &cand())).contains("never offered"));

        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop", 0.97),
            "desktop_number": choice("2", 0.99),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.01),
        }));
        let (got, _) = act(compose(&r, &cand()));
        assert_eq!(got.intent, Intent::VirtualDesktop { n: 2 });
    }

    #[test]
    fn unknown_direction_is_unclear_not_next() {
        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop_rel", 0.97),
            "desktop_direction": choice("sideways", 0.99),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.01),
        }));
        unclear(compose(&r, &cand()));

        let r = resp(serde_json::json!({
            "intent": choice("virtual_desktop_rel", 0.97),
            "desktop_direction": choice("previous", 0.99),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.01),
        }));
        let (got, _) = act(compose(&r, &cand()));
        assert_eq!(got.intent, Intent::VirtualDesktopRel { delta: -1 });
    }

    #[test]
    fn unoffered_choice_is_rejected() {
        // The model returning a bare app name instead of a key is a value we
        // never sent; it must not resolve to anything.
        let r = resp(serde_json::json!({
            "intent": choice("show_app", 0.94),
            "target": choice("Firefox", 0.99),
            "is_dictation": dictation(0.02),
            "is_destructive": noul(0.03),
        }));
        assert!(unclear(compose(&r, &cand())).contains("never offered"));

        let r = resp(serde_json::json!({
            "intent": choice("reboot", 0.94),
            "is_dictation": dictation(0.02),
            "is_destructive": noul(0.03),
        }));
        assert!(unclear(compose(&r, &cand())).contains("never offered"));
    }

    #[test]
    fn optional_model_absent_leaves_the_default() {
        let r = resp(serde_json::json!({
            "intent": choice("start_claude", 0.96),
            "claude_model": choice(NO_TARGET, 0.98),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.02),
        }));
        let (got, decision) = act(compose(&r, &cand()));
        assert_eq!(got.intent, Intent::StartClaude { model: None });
        assert_eq!(decision, Decision::Act);
    }

    #[test]
    fn payload_cue_is_stripped_by_rule_and_claude_tell_confirms() {
        // The cue rule finds the message; whatever span the model picked
        // is not consulted, and neither is its confidence.
        let c = cand_with(&[], &[], 1, "tell claude to rerun the failing test");
        assert!(!c.second_round("claude_tell").contains_key("payload"));
        let r = resp(serde_json::json!({
            "intent": choice("claude_tell", 0.93),
            "payload": choice("p:1", 0.3),
            "is_dictation": dictation(0.2),
        }));
        let (got, decision) = act(compose(&r, &c));
        assert_eq!(
            got.intent,
            Intent::ClaudeTell {
                text: "rerun the failing test".into()
            }
        );
        assert_eq!(got.confidence, 0.93);
        assert!(matches!(decision, Decision::Confirm { .. }));
    }

    #[test]
    fn payload_falls_back_to_the_model_span_when_no_cue_applies() {
        let c = cand_with(&[], &[], 1, "let claude know the tests are green");
        let second = c.second_round("claude_tell");
        assert!(second.contains_key("payload"));
        let offered = second["payload"].options().unwrap();
        assert!(!offered.contains_key("p:0"), "the first word is never a message");
        assert_eq!(offered.keys().last().map(String::as_str), Some(WHOLE));
        let r = resp(serde_json::json!({
            "intent": choice("claude_tell", 0.93),
            "payload": choice("p:3", 0.8),
            "is_dictation": dictation(0.02),
        }));
        let (got, _) = act(compose(&r, &c));
        assert_eq!(
            got.intent,
            Intent::ClaudeTell {
                text: "the tests are green".into()
            }
        );
        assert_eq!(got.confidence, 0.8);

        let whole = resp(serde_json::json!({
            "intent": choice("notify", 0.93),
            "payload": choice(WHOLE, 0.8),
            "is_dictation": dictation(0.02),
        }));
        let (got, _) = act(compose(&whole, &c));
        assert_eq!(
            got.intent,
            Intent::Notify {
                text: "let claude know the tests are green".into()
            }
        );
    }

    #[test]
    fn key_chord_is_mapped_by_rule_or_the_intent_is_unclear() {
        let c = cand_with(&[], &[], 1, "hit control s");
        let r = resp(serde_json::json!({
            "intent": choice("key", 0.97),
            "is_dictation": dictation(0.01),
        }));
        let (got, decision) = act(compose(&r, &c));
        assert_eq!(
            got.intent,
            Intent::Key {
                chord: "ctrl+s".into()
            }
        );
        assert!(matches!(decision, Decision::Confirm { .. }));

        // A word that is not a key never becomes a chord.
        let c = cand_with(&[], &[], 1, "press the any key");
        let r = resp(serde_json::json!({
            "intent": choice("key", 0.97),
            "is_dictation": dictation(0.01),
        }));
        assert!(unclear(compose(&r, &c)).contains("not a key chord"));
    }

    #[test]
    fn risk_is_the_intents_own_where_the_intent_decides_it() {
        // Starting Claude Code cannot destroy work, whatever the model
        // says, and the question is not even asked.
        let c = cand();
        assert!(!c.second_round("start_claude").contains_key("is_destructive"));
        let r = resp(serde_json::json!({
            "intent": choice("start_claude", 0.99),
            "claude_model": choice(NO_TARGET, 0.99),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(1.0),
        }));
        let (got, decision) = act(compose(&r, &c));
        assert_eq!(
            got.signals,
            Signals::Judged {
                confidence: 0.99,
                destructive: Some(0.0),
                dictation: Some(0.01),
            }
        );
        assert_eq!(decision, Decision::Act);

        // A search is the model's to judge, and its answer is kept.
        let c = cand_with(&[], &[], 1, "look for the invoice");
        assert!(c.second_round("krunner").contains_key("is_destructive"));
        let r = resp(serde_json::json!({
            "intent": choice("krunner", 0.99),
            "is_dictation": dictation(0.01),
            "is_destructive": noul(0.9),
        }));
        let (got, decision) = act(compose(&r, &c));
        assert!(matches!(got.signals, Signals::Judged { destructive: Some(d), .. } if d == 0.9));
        assert!(matches!(decision, Decision::Confirm { .. }));
    }

    #[test]
    fn rounds_ask_only_what_the_intent_needs() {
        let c = cand_with(&[], &[], 2, "remind me to buy milk");
        let first = c.first_round();
        assert_eq!(first.keys().collect::<Vec<_>>(), ["intent", "is_dictation"]);
        assert!(c.second_round("notify").is_empty(), "cue found, risk never");
        assert!(c.second_round("open_terminal").is_empty());
        assert!(c.second_round(NO_TARGET).is_empty());
        assert_eq!(
            c.second_round("show_app").keys().collect::<Vec<_>>(),
            ["target"]
        );
        assert_eq!(
            c.second_round("virtual_desktop").keys().collect::<Vec<_>>(),
            ["desktop_number"]
        );
        assert_eq!(
            c.second_round("virtual_desktop_rel").keys().collect::<Vec<_>>(),
            ["desktop_direction"]
        );
        assert_eq!(
            c.second_round("claude_model").keys().collect::<Vec<_>>(),
            ["claude_model"]
        );
        assert_eq!(
            c.second_round("edit_text").keys().collect::<Vec<_>>(),
            ["is_destructive"]
        );
        assert!(c.second_round("key").contains_key("payload"), "no key cue in this utterance");
    }

    #[test]
    fn krunner_searches_for_the_payload() {
        let c = cand_with(&["Firefox"], &[], 1, "search for quarterly report");
        let r = resp(serde_json::json!({
            "intent": choice("krunner", 0.93),
            "target": choice("a:0", 0.9),
            "payload": choice("p:2", 0.9),
            "is_dictation": dictation(0.02),
            "is_destructive": noul(0.01),
        }));
        let (got, _) = act(compose(&r, &c));
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
            "is_dictation": dictation(0.02),
            "is_destructive": noul(0.02),
        }));
        let (got, _) = act(compose(&r, &c));
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
            "is_dictation": dictation(0.02),
            "is_destructive": noul(0.02),
        }));
        let (got, _) = act(compose(&r, &c));
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
            "is_dictation": dictation(0.02),
            "is_destructive": noul(0.02),
        }));
        let (got, _) = act(compose(&r, &c));
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
            "is_dictation": dictation(0.02),
            "is_destructive": noul(0.02),
        }));
        let (got, _) = act(compose(&r, &c));
        assert_eq!(
            got.intent,
            Intent::FocusWindow {
                query: "build — Konsole".into()
            }
        );
        assert_eq!(got.window_id.as_deref(), Some("{k1}"));
        assert_eq!(got.entry_id, None);
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
            last_dictation: false,
        };
        let sent = |send_window_titles: bool| {
            let c = Candidates::build("bring up kate", &ctx, send_window_titles);
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
            last_dictation: false,
        };
        let utterance = vec!["word"; 300].join(" ");
        let c = Candidates::build(&utterance, &ctx, false);
        assert_eq!(c.windows.len(), WINDOW_CANDIDATES);
        assert_eq!(c.spans.len(), PAYLOAD_SPANS);
        assert!(c.spans[0].split_whitespace().count() > PAYLOAD_SPANS, "the first span runs to the end of the utterance");
        for (id, q) in &c.questions {
            if let Some(o) = q.options() {
                assert!(o.len() <= MAX_CHOICE_OPTIONS, "{id} offers {}", o.len());
            }
        }
        let n = c.questions["desktop_number"].options().unwrap().len();
        assert_eq!(n, MAX_CHOICE_OPTIONS);
    }
}
