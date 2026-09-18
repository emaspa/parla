//! `parlad --calibrate [corpus.toml]`: run a corpus of utterances through
//! the judged path against a synthetic desktop and measure the thresholds.
//!
//! The corpus describes a desktop (installed applications, open windows,
//! desktops) and a list of cases, each an utterance with what the judge
//! ought to make of it. Every case goes through the same `judge_verdict`
//! the daemon uses, against the same model, and the report says how often
//! the intent and its arguments were right, how the confidence separates
//! right from wrong, and which thresholds would score best.
//!
//! Nothing here touches the desktop: no executor, no bus, no notification.
//! The state is the corpus's, not the machine's.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use desktopd::{DesktopEntry, DesktopIndex, Window};
use parla_grammar::Intent;
use serde::Deserialize;

use crate::config::DaemonConfig;
use crate::judge::{Context, Detailed, Judge, Verdict};
use crate::policy::{intent_name, Decision, Policy, Signals};

/// Expected-value spellings that are not intent names.
const DICTATION: &str = "dictation";
const UNCLEAR: &str = "unclear";
/// A target that means "the window with focus, no name given".
const FOCUSED: &str = "focused";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    state: State,
    #[serde(default, rename = "case")]
    cases: Vec<Case>,
}

/// The synthetic desktop every case is judged against.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    desktop_count: u32,
    current_desktop: u32,
    #[serde(default)]
    claude_running: bool,
    #[serde(default)]
    apps: Vec<App>,
    #[serde(default)]
    windows: Vec<Win>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct App {
    id: String,
    name: String,
    generic_name: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Win {
    id: String,
    title: String,
    class: String,
    #[serde(default = "one")]
    desktop: i32,
}

fn one() -> i32 {
    1
}

/// One utterance and what the judge ought to make of it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    say: String,
    /// An intent name as the judge uses them, or `dictation`, or `unclear`.
    expect: String,
    /// Other intent names that are also acceptable, for the few utterances
    /// where two readings are equally right.
    #[serde(default)]
    accept: Vec<String>,
    /// Substring of the window title, window class, application name or
    /// desktop id that must be resolved; `focused` for the focused window.
    target: Option<String>,
    desktop: Option<u32>,
    /// `next` or `previous`.
    direction: Option<String>,
    /// The text a `claude_tell`, `notify`, `krunner` or `key` must carry.
    payload: Option<String>,
    /// The Claude model a `start_claude` or `claude_model` must name.
    model: Option<String>,
    /// Judge as if text was dictated a moment ago, so `edit_text` is offered.
    #[serde(default)]
    last_dictation: bool,
    /// Whether the model ought to flag this as destructive.
    destructive: Option<bool>,
}

/// What one judged case came to.
struct Outcome<'a> {
    case: &'a Case,
    /// The intent name the verdict built, `dictation`, `unclear`, or the
    /// model's first-ranked intent key when nothing could be built.
    got: String,
    /// The built intent's target/argument, for the report line.
    detail: String,
    /// Intent (or refusal kind) as expected.
    intent_ok: bool,
    /// Every argument the case names as expected. Only meaningful with
    /// `intent_ok`; a wrong intent has no right arguments.
    args_ok: bool,
    /// Whether a target was named and resolved as expected.
    target_ok: Option<bool>,
    confidence: Option<f64>,
    dictation: Option<f64>,
    destructive: Option<f64>,
    /// The resolved intent and its signals, when a verdict was built.
    act: Option<(Intent, Signals)>,
}

impl Outcome<'_> {
    /// Right in every respect the case specifies.
    fn right(&self) -> bool {
        self.intent_ok && self.args_ok && self.target_ok.unwrap_or(true)
    }

    fn expects_action(&self) -> bool {
        expects_action(self.case)
    }

    /// What the daemon would do with this case under `policy`.
    fn decide(&self, policy: &Policy) -> Decision {
        if policy.is_prose(self.dictation) {
            return Decision::Refuse {
                reason: "dictation".into(),
            };
        }
        match &self.act {
            Some((intent, signals)) => policy.decide(intent, signals),
            None => Decision::Refuse {
                reason: "unclear".into(),
            },
        }
    }
}

pub async fn run(path: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading corpus {}", path.display()))?;
    let corpus: Corpus =
        toml::from_str(&text).with_context(|| format!("parsing corpus {}", path.display()))?;
    anyhow::ensure!(!corpus.cases.is_empty(), "the corpus has no cases");
    anyhow::ensure!(
        corpus.state.desktop_count >= 1
            && (1..=corpus.state.desktop_count).contains(&corpus.state.current_desktop),
        "state.current_desktop must be within 1..=state.desktop_count"
    );
    for c in &corpus.cases {
        check_case(c)?;
    }

    let cfg = DaemonConfig::load()?;
    anyhow::ensure!(cfg.judge.enabled, "judge.enabled = false in the config");
    let grammar = crate::load_grammar(&cfg)?;
    let local = crate::load_local(&cfg).await;
    let judge = Arc::new(Judge::new(&cfg.judge, local)?);
    let policy = judge.policy().clone();

    let index = DesktopIndex::from_entries(
        corpus
            .state
            .apps
            .iter()
            .map(|a| DesktopEntry {
                id: a.id.clone(),
                name: a.name.clone(),
                generic_name: a.generic_name.clone(),
                keywords: a.keywords.iter().map(|k| k.to_lowercase()).collect(),
                exec: None,
                terminal: false,
            })
            .collect(),
    );
    let windows: Vec<Window> = corpus
        .state
        .windows
        .iter()
        .enumerate()
        .map(|(i, w)| Window {
            id: w.id.clone(),
            title: w.title.clone(),
            class: w.class.clone(),
            resource_name: w.class.clone(),
            desktop: w.desktop,
            active: false,
            minimized: false,
            stacking: i as i32,
            pid: 0,
            normal: true,
        })
        .collect();

    println!("corpus:  {} ({} cases)", path.display(), corpus.cases.len());
    println!("judge:   {}", judge.describe());
    println!(
        "state:   {} apps, {} windows, desktop {}/{}, claude {}, thresholds min {:.2} act {:.2} dictation {:.2} destructive {:.2}",
        corpus.state.apps.len(),
        windows.len(),
        corpus.state.current_desktop,
        corpus.state.desktop_count,
        if corpus.state.claude_running { "running" } else { "not running" },
        policy.min_confidence,
        policy.act_unconfirmed_above,
        policy.dictation_threshold,
        policy.destructive_threshold,
    );
    println!();

    let mut outcomes: Vec<Outcome<'_>> = Vec::new();
    let mut grammar_matched = 0;
    let total = corpus.cases.len();
    for (i, case) in corpus.cases.iter().enumerate() {
        eprintln!("[{}/{total}] {:?}", i + 1, case.say);
        if let Some(intent) = grammar.parse(&case.say) {
            grammar_matched += 1;
            println!(
                "skip  grammar matched {:<24} {:?}",
                intent_name(&intent),
                case.say
            );
            continue;
        }
        let ctx = Context {
            windows: &windows,
            index: &index,
            current_desktop: corpus.state.current_desktop,
            desktop_count: corpus.state.desktop_count,
            claude_running: corpus.state.claude_running,
            last_dictation: case.last_dictation,
        };
        let detailed = judge
            .judge_verdict_detailed(&case.say, &ctx)
            .await
            .with_context(|| format!("judging {:?}", case.say))?;
        let outcome = assess(case, detailed, &policy);
        print_line(&outcome, &policy);
        outcomes.push(outcome);
    }
    println!();
    if grammar_matched > 0 {
        println!(
            "{grammar_matched} case(s) matched the grammar and were skipped: they never reach the judge."
        );
    }
    anyhow::ensure!(!outcomes.is_empty(), "no case reached the judge");
    summarize(&outcomes, &policy);
    Ok(())
}

fn expects_action(case: &Case) -> bool {
    case.expect != DICTATION && case.expect != UNCLEAR
}

fn check_case(c: &Case) -> anyhow::Result<()> {
    const INTENTS: &[&str] = &[
        "show_app",
        "close_window",
        "minimize_window",
        "maximize_window",
        "virtual_desktop",
        "virtual_desktop_rel",
        "open_terminal",
        "start_claude",
        "claude_model",
        "claude_tell",
        "claude_read",
        "krunner",
        "notify",
        "key",
        "edit_text",
    ];
    anyhow::ensure!(!c.say.trim().is_empty(), "a case has an empty `say`");
    for name in std::iter::once(&c.expect).chain(&c.accept) {
        anyhow::ensure!(
            INTENTS.contains(&name.as_str()) || name == DICTATION || name == UNCLEAR,
            "case {:?}: unknown expectation {name:?}",
            c.say
        );
    }
    if let Some(d) = &c.direction {
        anyhow::ensure!(
            d == "next" || d == "previous",
            "case {:?}: direction must be next or previous",
            c.say
        );
    }
    if c.expect == "edit_text" {
        anyhow::ensure!(
            c.last_dictation,
            "case {:?}: edit_text needs last_dictation = true",
            c.say
        );
    }
    Ok(())
}

/// The judge's name for a built intent: launch and focus are both
/// `show_app` to the judge, since code split them afterwards.
fn judged_name(intent: &Intent) -> &'static str {
    match intent {
        Intent::LaunchApp { .. } | Intent::FocusWindow { .. } => "show_app",
        other => intent_name(other),
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The payload as spoken may keep a wrapper word the case leaves out:
/// "to fix the test" for "fix the test". Either is the same message.
fn payload_matches(got: &str, want: &str) -> bool {
    let got = norm(got);
    let want = norm(want);
    got == want
        || ["to ", "that ", "the "]
            .iter()
            .any(|w| got.strip_prefix(w).is_some_and(|rest| rest == want))
}

/// Judge the verdict against the case. A case that expects `dictation` or
/// `unclear` is right when the daemon would refuse it: the model's
/// `is_dictation` clears the current threshold, or no intent could be
/// built. Whether prose was recognised as prose is reported separately.
fn assess<'a>(case: &'a Case, d: Detailed, policy: &Policy) -> Outcome<'a> {
    let ranked = d
        .intent
        .as_ref()
        .map(|(k, p)| format!("{k} {p:.2}"))
        .unwrap_or_else(|| "no intent".into());
    let prose = policy.is_prose(d.dictation);
    let accepts = |name: &str| case.expect == name || case.accept.iter().any(|a| a == name);
    match d.verdict {
        Verdict::Act(r) => {
            let name = judged_name(&r.intent);
            let intent_ok = match case.expect.as_str() {
                DICTATION | UNCLEAR => prose,
                _ => accepts(name),
            };
            let (detail, target_ok) = check_target(
                case,
                &r.intent,
                r.window_id.as_deref(),
                r.entry_id.as_deref(),
            );
            let args_ok = intent_ok && check_args(case, &r.intent);
            let target_ok = if intent_ok {
                target_ok
            } else {
                case.target.as_ref().map(|_| false)
            };
            Outcome {
                case,
                got: name.to_string(),
                detail,
                intent_ok,
                args_ok,
                target_ok,
                confidence: Some(r.confidence),
                dictation: d.dictation,
                destructive: d.destructive,
                act: Some((r.intent, r.signals)),
            }
        }
        Verdict::Dictation => {
            unreachable!("judge_verdict_detailed leaves the dictation gate to the caller")
        }
        Verdict::Unclear(reason) => Outcome {
            case,
            got: UNCLEAR.into(),
            detail: format!("{ranked}: {reason}"),
            intent_ok: !expects_action(case),
            args_ok: true,
            target_ok: None,
            confidence: None,
            dictation: d.dictation,
            destructive: d.destructive,
            act: None,
        },
    }
}

/// The built intent's target, described, and whether it is the one the
/// case names.
fn check_target(
    case: &Case,
    intent: &Intent,
    window_id: Option<&str>,
    entry_id: Option<&str>,
) -> (String, Option<bool>) {
    let query: Option<Option<&str>> = match intent {
        Intent::LaunchApp { query } | Intent::FocusWindow { query } => Some(Some(query)),
        Intent::CloseWindow { query }
        | Intent::MinimizeWindow { query }
        | Intent::MaximizeWindow { query } => Some(query.as_deref()),
        _ => None,
    };
    let detail = match (query, intent) {
        (Some(Some(q)), _) => format!("{q:?}"),
        (Some(None), _) => "focused window".into(),
        (None, Intent::VirtualDesktop { n }) => format!("desktop {n}"),
        (None, Intent::VirtualDesktopRel { delta }) => {
            if *delta > 0 {
                "next".into()
            } else {
                "previous".into()
            }
        }
        (None, Intent::StartClaude { model }) => {
            model.clone().unwrap_or_else(|| "default model".into())
        }
        (None, Intent::ClaudeModel { model }) => model.clone(),
        (None, Intent::ClaudeTell { text }) | (None, Intent::Notify { text }) => {
            format!("{text:?}")
        }
        (None, Intent::KRunner { query }) => format!("{query:?}"),
        (None, Intent::Key { chord }) => format!("{chord:?}"),
        (None, Intent::EditText { instruction }) => format!("{instruction:?}"),
        (None, _) => String::new(),
    };
    let Some(want) = &case.target else {
        return (detail, None);
    };
    let Some(query) = query else {
        return (detail, Some(false));
    };
    let want = norm(want);
    let ok = match query {
        None => want == FOCUSED,
        Some(q) => {
            want != FOCUSED
                && (norm(q).contains(&want)
                    || window_id.is_some_and(|id| norm(id) == want)
                    || entry_id.is_some_and(|id| norm(id).contains(&want)))
        }
    };
    (detail, Some(ok))
}

/// The arguments the case names, against the built intent. Each is checked
/// on the intent it belongs to, so a case that accepts two readings can
/// name what each of them must carry.
fn check_args(case: &Case, intent: &Intent) -> bool {
    match intent {
        Intent::VirtualDesktop { n } => case.desktop.is_none_or(|want| *n == want),
        Intent::VirtualDesktopRel { delta } => case
            .direction
            .as_deref()
            .is_none_or(|dir| *delta == if dir == "next" { 1 } else { -1 }),
        Intent::ClaudeTell { text } | Intent::Notify { text } => case
            .payload
            .as_deref()
            .is_none_or(|p| payload_matches(text, p)),
        Intent::KRunner { query } => case
            .payload
            .as_deref()
            .is_none_or(|p| payload_matches(query, p)),
        Intent::Key { chord } => case
            .payload
            .as_deref()
            .is_none_or(|p| payload_matches(chord, p)),
        Intent::StartClaude { model } => case
            .model
            .as_deref()
            .is_none_or(|m| model.as_deref().map(norm) == Some(norm(m))),
        Intent::ClaudeModel { model } => {
            case.model.as_deref().is_none_or(|m| norm(model) == norm(m))
        }
        _ => true,
    }
}

fn decision_word(d: &Decision) -> String {
    match d {
        Decision::Act => "act".into(),
        Decision::Confirm { .. } => "ask".into(),
        Decision::Refuse { reason } => {
            format!("refuse({})", reason.split(' ').next().unwrap_or(""))
        }
    }
}

fn print_line(o: &Outcome<'_>, policy: &Policy) {
    let status = if o.right() { "ok   " } else { "WRONG" };
    let target = match o.target_ok {
        Some(true) => "target ok ",
        Some(false) => "target BAD",
        None => "          ",
    };
    let conf = o
        .confidence
        .map(|c| format!("{c:.2}"))
        .unwrap_or_else(|| "  - ".into());
    println!(
        "{status} {:<19} {:<19} {target} conf {conf} dict {:.2} destr {:.2} {:<20} {:?}  [{}]",
        o.case.expect,
        o.got,
        o.dictation.unwrap_or(f64::NAN),
        o.destructive.unwrap_or(f64::NAN),
        decision_word(&o.decide(policy)),
        o.case.say,
        o.detail,
    );
}

/// Score of a threshold pair over the corpus: the brief's formula.
fn score(outcomes: &[Outcome<'_>], policy: &Policy) -> i64 {
    let mut s = 0;
    for o in outcomes {
        let right = o.right();
        match (o.decide(policy), right) {
            (Decision::Act, true) => s += 1,
            (Decision::Act, false) => s -= 5,
            (Decision::Confirm { .. }, true) => s -= 1,
            (Decision::Refuse { .. }, true) if o.expects_action() => s -= 2,
            _ => {}
        }
    }
    s
}

fn grid() -> Vec<f64> {
    (1..=19).map(|i| f64::from(i) * 0.05).collect()
}

fn histogram(title: &str, values: &[f64]) {
    let mut buckets = [0usize; 10];
    for &v in values {
        let i = ((v * 10.0).floor() as usize).min(9);
        buckets[i] += 1;
    }
    println!("  {title} ({} values)", values.len());
    for (i, n) in buckets.iter().enumerate() {
        println!(
            "    {:.1}-{:.1} {:>3} {}",
            i as f64 / 10.0,
            (i + 1) as f64 / 10.0,
            n,
            "#".repeat(*n)
        );
    }
}

/// Precision and recall of `p >= t` for the positives.
fn pr(pairs: &[(f64, bool)], t: f64) -> (f64, f64, usize, usize) {
    let tp = pairs.iter().filter(|(p, pos)| *pos && *p >= t).count();
    let fp = pairs.iter().filter(|(p, pos)| !*pos && *p >= t).count();
    let fn_ = pairs.iter().filter(|(p, pos)| *pos && *p < t).count();
    let precision = if tp + fp == 0 {
        1.0
    } else {
        tp as f64 / (tp + fp) as f64
    };
    let recall = if tp + fn_ == 0 {
        1.0
    } else {
        tp as f64 / (tp + fn_) as f64
    };
    (precision, recall, fp, fn_)
}

/// Which threshold to pick when several score the same F1.
#[derive(Clone, Copy)]
enum Tie {
    /// The middle of the tied range, so the choice is not on the edge of
    /// a gap.
    Middle,
    /// The lowest, for a signal where a miss costs more than a false
    /// alarm.
    Low,
}

/// The threshold with the best F1 over the grid.
fn best_threshold(pairs: &[(f64, bool)], tie: Tie) -> f64 {
    let scored: Vec<(f64, f64)> = grid()
        .into_iter()
        .map(|t| {
            let (p, r, _, _) = pr(pairs, t);
            let f1 = if p + r == 0.0 {
                0.0
            } else {
                2.0 * p * r / (p + r)
            };
            (t, f1)
        })
        .collect();
    let best = scored.iter().map(|(_, f)| *f).fold(f64::MIN, f64::max);
    let tied: Vec<f64> = scored
        .iter()
        .filter(|(_, f)| (*f - best).abs() < 1e-9)
        .map(|(t, _)| *t)
        .collect();
    match tie {
        Tie::Middle => tied[tied.len() / 2],
        Tie::Low => tied[0],
    }
}

fn summarize(outcomes: &[Outcome<'_>], policy: &Policy) {
    println!("== Summary ==");
    let n = outcomes.len();
    let right = outcomes.iter().filter(|o| o.right()).count();
    let actions: Vec<&Outcome<'_>> = outcomes.iter().filter(|o| o.expects_action()).collect();
    let intent_right = actions.iter().filter(|o| o.intent_ok).count();
    let args_right = actions.iter().filter(|o| o.intent_ok && o.args_ok).count();
    let targeted: Vec<&Outcome<'_>> = outcomes
        .iter()
        .filter(|o| o.case.target.is_some())
        .collect();
    let target_right = targeted
        .iter()
        .filter(|o| o.target_ok == Some(true))
        .count();
    let refusals: Vec<&Outcome<'_>> = outcomes.iter().filter(|o| !o.expects_action()).collect();
    let refusal_right = refusals.iter().filter(|o| o.right()).count();
    let prose: Vec<&Outcome<'_>> = outcomes
        .iter()
        .filter(|o| o.case.expect == DICTATION)
        .collect();
    let prose_as_prose = prose
        .iter()
        .filter(|o| policy.is_prose(o.dictation))
        .count();
    let prose_refused = prose.iter().filter(|o| o.right()).count();
    println!("judged cases:        {n}");
    println!(
        "right in every respect: {right}/{n} ({:.0}%)",
        100.0 * right as f64 / n as f64
    );
    println!(
        "intent accuracy:     {intent_right}/{} ({:.0}%) on cases that expect an action; with arguments {args_right}/{}",
        actions.len(),
        100.0 * intent_right as f64 / actions.len().max(1) as f64,
        actions.len(),
    );
    println!(
        "target accuracy:     {target_right}/{} ({:.0}%) on cases that name a target",
        targeted.len(),
        100.0 * target_right as f64 / targeted.len().max(1) as f64,
    );
    println!(
        "refusals:            {refusal_right}/{} ({:.0}%) of cases that expect a refusal got one (unclear verdict, or model called it prose)",
        refusals.len(),
        100.0 * refusal_right as f64 / refusals.len().max(1) as f64,
    );
    println!(
        "prose:               {prose_refused}/{} refused, {prose_as_prose} of them recognised as dictation",
        prose.len()
    );

    // Dictation detection at the current threshold and across the grid.
    let dict_pairs: Vec<(f64, bool)> = outcomes
        .iter()
        .filter_map(|o| o.dictation.map(|p| (p, o.case.expect == DICTATION)))
        .collect();
    let (p, r, fp, fn_) = pr(&dict_pairs, policy.dictation_threshold);
    println!(
        "dictation detection: precision {p:.2} recall {r:.2} at dictation_threshold {:.2} ({fp} commands called prose, {fn_} prose missed)",
        policy.dictation_threshold
    );
    println!();

    // Confidence distribution for right vs wrong verdicts.
    println!("== Confidence of built verdicts ==");
    let right_conf: Vec<f64> = outcomes
        .iter()
        .filter(|o| o.right())
        .filter_map(|o| o.confidence)
        .collect();
    let wrong_conf: Vec<f64> = outcomes
        .iter()
        .filter(|o| !o.right())
        .filter_map(|o| o.confidence)
        .collect();
    histogram("right", &right_conf);
    histogram(
        "wrong (wrong intent, argument or target, or should have been refused)",
        &wrong_conf,
    );
    println!();

    // Sweep min_confidence and act_unconfirmed_above.
    println!("== Threshold sweep ==");
    println!("score = correct unconfirmed acts - 5 * wrong unconfirmed acts - correct-but-asked - 2 * correct-but-refused");
    println!("(dictation and destructive thresholds held at the current values)");
    let mut table: BTreeMap<(u32, u32), i64> = BTreeMap::new();
    for min in grid() {
        for act in grid() {
            if min > act + 1e-9 {
                continue;
            }
            let p = Policy {
                min_confidence: min,
                act_unconfirmed_above: act,
                ..policy.clone()
            };
            table.insert((key(min), key(act)), score(outcomes, &p));
        }
    }
    let best = table.values().copied().max().unwrap_or(0);
    let tied: Vec<(u32, u32)> = table
        .iter()
        .filter(|(_, s)| **s == best)
        .map(|(k, _)| *k)
        .collect();
    // The middle of the tied region, so the choice is not on its edge.
    let mut mins: Vec<u32> = tied.iter().map(|(m, _)| *m).collect();
    mins.sort_unstable();
    mins.dedup();
    let best_min = mins[mins.len() / 2];
    let mut acts: Vec<u32> = tied
        .iter()
        .filter(|(m, _)| *m == best_min)
        .map(|(_, a)| *a)
        .collect();
    acts.sort_unstable();
    let best_act = acts[acts.len() / 2];
    let current = score(outcomes, policy);
    println!(
        "best score {best} at min_confidence {:.2}, act_unconfirmed_above {:.2} ({} pair(s) tie; min_confidence {:.2}..{:.2}, act_unconfirmed_above {:.2}..{:.2} at that floor); current thresholds score {current}",
        unkey(best_min),
        unkey(best_act),
        tied.len(),
        unkey(mins[0]),
        unkey(mins[mins.len() - 1]),
        unkey(acts[0]),
        unkey(acts[acts.len() - 1]),
    );
    let cols: Vec<u32> = (best_act.saturating_sub(15)..=best_act + 15)
        .step_by(5)
        .filter(|a| (5..=95).contains(a))
        .collect();
    print!("min \\ act ");
    for a in &cols {
        print!(" {:>5.2}", unkey(*a));
    }
    println!();
    for m in (best_min.saturating_sub(15)..=best_min + 15).step_by(5) {
        if !(5..=95).contains(&m) {
            continue;
        }
        print!("     {:>5.2}", unkey(m));
        for a in &cols {
            match table.get(&(m, *a)) {
                Some(s) => print!(" {s:>5}"),
                None => print!("     ."),
            }
        }
        println!();
    }
    let breakdown = |p: &Policy| {
        let mut act_ok = 0;
        let mut act_wrong = 0;
        let mut ask_ok = 0;
        let mut ask_wrong = 0;
        let mut refuse_ok = 0;
        for o in outcomes {
            match (o.decide(p), o.right()) {
                (Decision::Act, true) => act_ok += 1,
                (Decision::Act, false) => act_wrong += 1,
                (Decision::Confirm { .. }, true) => ask_ok += 1,
                (Decision::Confirm { .. }, false) => ask_wrong += 1,
                (Decision::Refuse { .. }, true) if o.expects_action() => refuse_ok += 1,
                _ => {}
            }
        }
        format!("acts right {act_ok}, acts WRONG {act_wrong}, asks right {ask_ok}, asks wrong {ask_wrong}, refuses right {refuse_ok}")
    };
    println!(
        "at the best pair: {}",
        breakdown(&Policy {
            min_confidence: unkey(best_min),
            act_unconfirmed_above: unkey(best_act),
            ..policy.clone()
        })
    );
    println!("at the current pair: {}", breakdown(policy));
    println!();

    // Dictation threshold.
    println!("== dictation_threshold ==");
    let prose: Vec<f64> = dict_pairs
        .iter()
        .filter(|(_, pos)| *pos)
        .map(|(p, _)| *p)
        .collect();
    let commands: Vec<f64> = dict_pairs
        .iter()
        .filter(|(_, pos)| !*pos)
        .map(|(p, _)| *p)
        .collect();
    histogram("is_dictation on prose cases", &prose);
    histogram("is_dictation on everything else", &commands);
    println!("  threshold  precision  recall  commands called prose  prose missed");
    for t in grid() {
        let (p, r, fp, fn_) = pr(&dict_pairs, t);
        println!("  {t:>9.2}  {p:>9.2}  {r:>6.2}  {fp:>21}  {fn_:>12}");
    }
    println!(
        "suggested dictation_threshold: {:.2} (best F1, middle of the tie)",
        best_threshold(&dict_pairs, Tie::Middle)
    );
    println!();

    // Destructive threshold, on the cases that say.
    println!("== destructive_threshold ==");
    let destr_pairs: Vec<(f64, bool)> = outcomes
        .iter()
        .filter_map(|o| Some((o.destructive?, o.case.destructive?)))
        .collect();
    if destr_pairs.is_empty() {
        println!("  no case sets `destructive`");
    } else {
        let yes: Vec<f64> = destr_pairs
            .iter()
            .filter(|(_, d)| *d)
            .map(|(p, _)| *p)
            .collect();
        let no: Vec<f64> = destr_pairs
            .iter()
            .filter(|(_, d)| !*d)
            .map(|(p, _)| *p)
            .collect();
        histogram("is_destructive on cases marked destructive", &yes);
        histogram("is_destructive on cases marked harmless", &no);
        let (p, r, fp, fn_) = pr(&destr_pairs, policy.destructive_threshold);
        println!(
            "  at destructive_threshold {:.2}: precision {p:.2} recall {r:.2} ({fp} harmless flagged, {fn_} destructive missed)",
            policy.destructive_threshold
        );
        println!(
            "  suggested destructive_threshold: {:.2} (best F1, low end of the tie: a miss costs more than a needless prompt)",
            best_threshold(&destr_pairs, Tie::Low)
        );
    }
}

fn key(t: f64) -> u32 {
    (t * 100.0).round() as u32
}

fn unkey(k: u32) -> f64 {
    f64::from(k) / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_tolerates_the_wrapper_word() {
        assert!(payload_matches("to fix the test", "fix the test"));
        assert!(payload_matches("Fix the  test", "fix the test"));
        assert!(!payload_matches("please fix the test", "fix the test"));
        assert!(!payload_matches("fix the test", "to fix the test"));
    }

    #[test]
    fn target_matches_title_class_or_id_and_focused_is_special() {
        let case = |target: &str| Case {
            say: String::new(),
            expect: "close_window".into(),
            accept: vec![],
            target: Some(target.into()),
            desktop: None,
            direction: None,
            payload: None,
            model: None,
            last_dictation: false,
            destructive: None,
        };
        let close = |q: Option<&str>| Intent::CloseWindow {
            query: q.map(String::from),
        };
        assert_eq!(
            check_target(
                &case("konsole"),
                &close(Some("build — Konsole")),
                Some("{w0}"),
                None
            )
            .1,
            Some(true)
        );
        assert_eq!(
            check_target(
                &case("{w0}"),
                &close(Some("build — Konsole")),
                Some("{w0}"),
                None
            )
            .1,
            Some(true)
        );
        assert_eq!(
            check_target(
                &case("kate"),
                &close(Some("build — Konsole")),
                Some("{w0}"),
                None
            )
            .1,
            Some(false)
        );
        assert_eq!(
            check_target(&case("focused"), &close(None), None, None).1,
            Some(true)
        );
        assert_eq!(
            check_target(&case("focused"), &close(Some("x")), None, None).1,
            Some(false)
        );
        assert_eq!(
            check_target(&case("kate"), &Intent::OpenTerminal, None, None).1,
            Some(false)
        );
        let launch = Intent::LaunchApp {
            query: "Firefox".into(),
        };
        assert_eq!(
            check_target(&case("firefox"), &launch, None, Some("firefox.desktop")).1,
            Some(true)
        );
    }

    #[test]
    fn best_threshold_sits_in_the_middle_of_a_tie() {
        // Positives at 0.9 and 1.0, negatives at 0.0 and 0.1: every
        // threshold from 0.15 to 0.90 separates them; the middle is chosen.
        let pairs = [(0.0, false), (0.1, false), (0.9, true), (1.0, true)];
        let t = best_threshold(&pairs, Tie::Middle);
        assert!((0.45..=0.6).contains(&t), "{t}");
        assert!((best_threshold(&pairs, Tie::Low) - 0.15).abs() < 1e-9);
    }
}
