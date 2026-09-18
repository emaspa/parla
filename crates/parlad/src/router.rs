//! The router: dictation goes through the flow (snippets, dictionary,
//! cleanup) and is typed into the focused window; commands go through the
//! fast-path grammar first and fall through to the judged path. Both
//! command paths end in the same [`Policy`] decision, and what the policy
//! wants confirmed waits in [`Confirmations`] for the next command-mode
//! utterance.
//!
//! The router also remembers the last dictation for a while, so "scratch
//! that" can take it back and "make that more formal" can rewrite it, and,
//! when the field could be read, reads it again a little later to see
//! whether the user corrected a word by hand; see [`Learn`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use desktopd::{
    Command, DesktopIndex, Executor, FocusedText, TextHandle, Verified, Window, WindowTarget,
};
use parla_flow::learned::corrections;
use parla_grammar::{Grammar, Intent};

use crate::command::{from_intent, from_resolved};
use crate::confirm::{check_target, Confirmations, Pending, Taken};
use crate::dbus::{Bus, State};
use crate::flow::{Flow, Processed, TextContext};
use crate::judge::{Context, Judge, Verdict};
use crate::policy::{Decision, Policy, Signals};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Transcript becomes keystrokes into whatever is focused.
    Dictate,
    /// Transcript is parsed as a command.
    Command,
}

/// Observed desktop state at one moment, gathered before judging. Every field
/// is a fact that was actually read; a query that fails refuses the judged
/// path instead of standing in a made-up value.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub windows: Vec<Window>,
    pub current_desktop: u32,
    pub desktop_count: u32,
    pub claude_running: bool,
    pub last_dictation: bool,
}

impl Snapshot {
    pub fn context<'a>(&'a self, index: &'a DesktopIndex) -> Context<'a> {
        Context {
            windows: &self.windows,
            index,
            current_desktop: self.current_desktop,
            desktop_count: self.desktop_count,
            claude_running: self.claude_running,
            last_dictation: self.last_dictation,
        }
    }
}

/// What handling an utterance came to. `Done` is a result to report;
/// `Confirm` is a question the user has to answer by voice, so the caller
/// should make sure it is heard; `Typed` is dictation that landed, with
/// what became of the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handled {
    Done(String),
    Confirm(String),
    Typed(Processed),
}

/// The dictation most recently typed, while it can still be taken back.
#[derive(Debug, Clone)]
struct LastDictation {
    text: String,
    window_id: String,
    class: String,
    at: Instant,
    /// Distinguishes this dictation from the ones before and after it, so
    /// a timer set for it cannot act on another.
    seq: u64,
    /// A correction pass still to run, when the field could be read.
    learn: Option<Learn>,
}

/// Characters of context kept on either side of a dictation, so the
/// comparison has anchors that were not dictated.
const LEARN_CONTEXT: usize = 20;
/// Extra characters read past the region, so a word the user added at the
/// end does not shift the context out of the read.
const LEARN_SLACK: i32 = 40;

/// What a correction pass needs: the field, and the region the dictation
/// occupies in it, as it was right after typing.
#[derive(Debug, Clone)]
struct Learn {
    handle: TextHandle,
    app: String,
    /// Context, the dictation, context.
    expected: String,
    /// Character offsets of `expected` in the field.
    from: i32,
    to: i32,
}

type Last = Arc<Mutex<Option<LastDictation>>>;

pub struct Router {
    executor: Arc<Executor>,
    grammar: Arc<Grammar>,
    judge: Option<Arc<Judge>>,
    flow: Arc<Flow>,
    policy: Policy,
    pending: Confirmations,
    last: Last,
    seq: AtomicU64,
    /// How long a confirmation prompt stays answerable.
    confirm_window: Duration,
    bus: Option<Bus>,
}

impl Router {
    /// The policy comes from the judge's config when there is one, so both
    /// paths share thresholds. Without a judge only grammar matches reach
    /// the gate, and those need no threshold.
    pub fn new(
        executor: Arc<Executor>,
        grammar: Arc<Grammar>,
        judge: Option<Arc<Judge>>,
        flow: Arc<Flow>,
        confirm_window: Duration,
        bus: Option<Bus>,
    ) -> Self {
        let policy = judge
            .as_ref()
            .map_or_else(Policy::default, |j| j.policy().clone());
        Self {
            executor,
            grammar,
            judge,
            flow,
            policy,
            pending: Confirmations::default(),
            last: Arc::new(Mutex::new(None)),
            seq: AtomicU64::new(0),
            confirm_window,
            bus,
        }
    }

    /// The thresholds both paths are gated by.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn flow(&self) -> &Flow {
        &self.flow
    }

    fn progress(&self, state: State, mode: Mode) {
        if let Some(bus) = &self.bus {
            bus.set_state(state, Some(mode));
        }
    }

    /// Handle a finished transcript. `focused` is the window that had focus
    /// when the capture started, so dictation is shaped for where it lands,
    /// and `field` the text field read at the same moment, when there was
    /// one to read.
    pub async fn handle(
        &self,
        mode: Mode,
        transcript: &str,
        focused: Option<&Window>,
        field: Option<&FocusedText>,
    ) -> anyhow::Result<Handled> {
        let transcript = transcript.trim();
        anyhow::ensure!(!transcript.is_empty(), "empty transcript");
        match mode {
            // Dictation is not an answer: a pending prompt survives it.
            Mode::Dictate => self.dictate(transcript, focused, field).await,
            Mode::Command => self.route_command(transcript, focused).await,
        }
    }

    async fn dictate(
        &self,
        transcript: &str,
        focused: Option<&Window>,
        field: Option<&FocusedText>,
    ) -> anyhow::Result<Handled> {
        let class = focused.map(|w| w.class.as_str()).unwrap_or_default();
        // A correction pass still waiting for the previous dictation runs
        // now, before this one changes the field.
        let pending = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .and_then(|l| l.learn.take());
        if let Some(learn) = pending {
            learn_from(&self.flow, learn).await;
        }
        let context = field
            .filter(|_| self.flow.context_enabled())
            .and_then(TextContext::from_focused);
        if let Some(c) = &context {
            tracing::debug!(
                "dictating into {} ({}), {} chars before the cursor",
                c.app,
                c.role,
                c.before.chars().count()
            );
        }
        self.progress(State::Thinking, Mode::Dictate);
        let processed = self.flow.process(transcript, class, context.as_ref()).await;
        self.progress(State::Typing, Mode::Dictate);
        self.executor.type_text(&processed.text).await?;
        let learn = match field {
            Some(f) if self.flow.learn_enabled() && !f.password => {
                Some(learn_region(f, &processed.text).await)
            }
            _ => None,
        };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let armed = learn.is_some();
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(LastDictation {
            text: processed.text.clone(),
            window_id: focused.map(|w| w.id.clone()).unwrap_or_default(),
            class: class.to_string(),
            at: Instant::now(),
            seq,
            learn,
        });
        if armed {
            let last = Arc::clone(&self.last);
            let flow = Arc::clone(&self.flow);
            let after = self.flow.learn_after();
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                // Gone if the dictation was scratched, edited or followed
                // by another one, which ran the pass itself.
                let learn = last
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut()
                    .filter(|l| l.seq == seq)
                    .and_then(|l| l.learn.take());
                if let Some(learn) = learn {
                    learn_from(&flow, learn).await;
                }
            });
        }
        Ok(Handled::Typed(processed))
    }

    /// The last dictation, if it is recent and the focus has not moved.
    fn recent_dictation(&self, focused: Option<&Window>) -> Option<LastDictation> {
        let last = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        if last.at.elapsed() > self.flow.edit_window() {
            return None;
        }
        let same_window = focused.is_none_or(|w| w.id == last.window_id);
        same_window.then_some(last)
    }

    /// Read the state the judged path needs. Public so `parlad --judge` can
    /// use the same observation instead of its own copy.
    pub async fn snapshot(&self, focused: Option<&Window>) -> anyhow::Result<Snapshot> {
        let windows = self
            .executor
            .list_windows()
            .await
            .context("cannot judge without the window list")?;
        let desktops = desktopd::kwin::list_desktops()
            .await
            .context("cannot judge without the virtual desktop list")?;
        anyhow::ensure!(!desktops.is_empty(), "KWin reports no virtual desktops");
        let current_desktop = desktopd::kwin::current_desktop()
            .await
            .context("cannot judge without knowing the current desktop")?;
        Ok(Snapshot {
            windows,
            current_desktop,
            desktop_count: desktops.len() as u32,
            claude_running: self.executor.tmux().session_exists().await,
            last_dictation: self.recent_dictation(focused).is_some(),
        })
    }

    async fn route_command(
        &self,
        transcript: &str,
        focused: Option<&Window>,
    ) -> anyhow::Result<Handled> {
        let intent = self.grammar.parse(transcript);
        // A reply is checked before anything else, so "yes" can never be
        // read as a command; any other command-mode utterance drops the
        // prompt, since the user has moved on.
        match intent {
            Some(Intent::Confirm) => return self.confirm().await,
            Some(Intent::Deny) => {
                return Ok(Handled::Done(match self.pending.cancel() {
                    Some(p) => format!("cancelled: {}", p.describe),
                    None => "nothing to cancel".into(),
                }))
            }
            _ => {
                if let Some(p) = self.pending.cancel() {
                    tracing::info!("dropped unconfirmed {}: new command spoken", p.describe);
                }
            }
        }

        if let Some(intent) = intent {
            tracing::info!("fast path: {intent:?}");
            if let Intent::ScratchThat = intent {
                return self.scratch(focused).await;
            }
            let command = from_intent(intent.clone())
                .context("grammar produced a reply where a command was expected")?;
            return self
                .dispatch("fast path", &intent, &Signals::Grammar, command)
                .await;
        }

        // Grammar matches literal word sequences, so anything phrased outside
        // its ~40 patterns lands here.
        let Some(judge) = &self.judge else {
            anyhow::bail!("no fast-path match for {transcript:?} (judged path disabled)");
        };
        self.route_judged(judge, transcript, focused).await
    }

    async fn route_judged(
        &self,
        judge: &Judge,
        transcript: &str,
        focused: Option<&Window>,
    ) -> anyhow::Result<Handled> {
        // Observed facts the judgment needs. Gathered here rather than inside
        // the judge so the judge stays a pure function of the state it is
        // given, and so a failure to observe is a router error, not a fact.
        let snapshot = self.snapshot(focused).await?;
        let ctx = snapshot.context(self.executor.desktop_index());

        self.progress(State::Thinking, Mode::Command);
        match judge.judge_verdict(transcript, &ctx).await? {
            Verdict::Act(resolved) => {
                tracing::info!(
                    "judged path: {:?} (confidence {:.2}, window {:?})",
                    resolved.intent,
                    resolved.confidence,
                    resolved.window_id
                );
                let source = format!("judged (confidence {:.2})", resolved.confidence);
                let intent = resolved.intent.clone();
                let signals = resolved.signals.clone();
                if let Intent::EditText { instruction } = &intent {
                    return self.edit(instruction, &signals, focused).await;
                }
                let command =
                    from_resolved(resolved).context("judge produced a reply, not a command")?;
                self.dispatch(&source, &intent, &signals, command).await
            }
            Verdict::Dictation => {
                anyhow::bail!(
                    "that sounded like dictation, not a command — hold the dictate hotkey instead"
                )
            }
            Verdict::Unclear(reason) => {
                anyhow::bail!("could not act on {transcript:?}: {reason}")
            }
        }
    }

    /// "scratch that": take back the last dictation, if it is still recent
    /// and the focus has not moved. The text before the cursor is checked
    /// first when the field can be read; a field that no longer ends with
    /// the dictation is left alone.
    async fn scratch(&self, focused: Option<&Window>) -> anyhow::Result<Handled> {
        let last = self
            .recent_dictation(focused)
            .ok_or_else(|| anyhow::anyhow!("nothing recent to take back"))?;
        self.progress(State::Typing, Mode::Command);
        let typed = last.text.chars().count();
        let verified = self.executor.verified_backspace(&last.text).await?;
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(Handled::Done(took_back(verified, typed)))
    }

    /// A spoken instruction about the last dictation: rewrite it with the
    /// cleanup model and replace what was typed. Editing one's own words of
    /// a moment ago is harmless and redoable, so a confident judgment acts
    /// without a prompt; an unconfident one is refused rather than asked.
    async fn edit(
        &self,
        instruction: &str,
        signals: &Signals,
        focused: Option<&Window>,
    ) -> anyhow::Result<Handled> {
        let last = self
            .recent_dictation(focused)
            .ok_or_else(|| anyhow::anyhow!("nothing recent to edit"))?;
        let intent = Intent::EditText {
            instruction: instruction.to_string(),
        };
        if let Decision::Refuse { reason } = self.policy.decide(&intent, signals) {
            anyhow::bail!("edit refused: {reason}");
        }
        self.progress(State::Thinking, Mode::Command);
        let text = self.flow.edit(&last.text, instruction, &last.class).await?;
        if text == last.text {
            return Ok(Handled::Done("nothing to change".into()));
        }
        self.progress(State::Typing, Mode::Command);
        let typed = last.text.chars().count();
        let verified = self.executor.verified_backspace(&last.text).await?;
        tracing::info!("edit: {}", took_back(verified, typed));
        self.executor.type_text(&text).await?;
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(LastDictation {
            text: text.clone(),
            at: Instant::now(),
            // Rewritten by parla, not corrected by hand: nothing to learn.
            learn: None,
            ..last
        });
        Ok(Handled::Typed(Processed {
            text,
            outcome: "edited",
            profile: String::new(),
        }))
    }

    /// The single gate. Voice is an unauthenticated input channel, so
    /// nothing the policy wants confirmed runs before the user has said yes
    /// to a prompt naming exactly what will happen.
    async fn dispatch(
        &self,
        source: &str,
        intent: &Intent,
        signals: &Signals,
        command: Command,
    ) -> anyhow::Result<Handled> {
        match self.policy.decide(intent, signals) {
            Decision::Act => {
                self.progress(State::Typing, Mode::Command);
                self.executor.execute(command).await.map(Handled::Done)
            }
            Decision::Confirm { reason } => self.ask(command, reason).await,
            Decision::Refuse { reason } => anyhow::bail!("{source} {intent:?} refused: {reason}"),
        }
    }

    /// Park `command` and word the question. A window target is resolved
    /// now, so the prompt names the window rather than the query, and so a
    /// later yes can check it still means the same window.
    async fn ask(&self, command: Command, reason: String) -> anyhow::Result<Handled> {
        let window = match command.window_target() {
            Some(target) => Some(self.executor.resolve_target(target).await?),
            None => None,
        };
        let describe = command.describe(window.as_ref().map(|w| w.title.as_str()));
        tracing::info!("asking to confirm: {describe} ({reason})");
        let replaced = self.pending.arm(Pending {
            command,
            window_id: window.map(|w| w.id),
            reason,
            describe: describe.clone(),
            deadline: Instant::now() + self.confirm_window,
        });
        if let Some(p) = replaced {
            tracing::info!("dropped unconfirmed {}: newer prompt", p.describe);
        }
        Ok(Handled::Confirm(format!(
            "{}? say yes",
            capitalize(&describe)
        )))
    }

    /// "yes": run the parked command if it is still in time and its window
    /// is still the one the prompt named.
    async fn confirm(&self) -> anyhow::Result<Handled> {
        let pending = match self.pending.take(Instant::now()) {
            Taken::Nothing => anyhow::bail!("nothing to confirm"),
            Taken::Expired(p) => {
                anyhow::bail!("too late to confirm {} (say the command again)", p.describe)
            }
            Taken::Live(p) => p,
        };
        let command = match pending.command.window_target() {
            Some(target) => {
                // Re-resolve rather than trust the id: a query must still
                // pick the same window, an id must still exist, and the
                // focused window must still be the one described.
                let current = self.executor.resolve_target(target).await.ok();
                check_target(&pending, current.as_ref())?;
                let id = current.map(|w| w.id).unwrap_or_default();
                pending.command.with_window_target(WindowTarget::Id(id))
            }
            None => pending.command,
        };
        tracing::info!("confirmed: {} ({})", pending.describe, pending.reason);
        self.progress(State::Typing, Mode::Command);
        self.executor.execute(command).await.map(Handled::Done)
    }
}

/// Where the dictation sits in the field once typed, with up to
/// [`LEARN_CONTEXT`] characters of what was already there on either side.
/// The caret after typing is read back when the application has processed
/// the keystrokes by then, and computed from the text otherwise.
async fn learn_region(field: &FocusedText, typed: &str) -> Learn {
    let before: Vec<char> = field.before.chars().collect();
    let k = before.len().min(LEARN_CONTEXT);
    let before_tail: String = before[before.len() - k..].iter().collect();
    let after_head: String = field.after.chars().take(LEARN_CONTEXT).collect();
    let m = after_head.chars().count();
    let computed = field.caret + typed.chars().count() as i32;
    let caret_after = match field.handle.caret().await {
        Ok(c) if c >= computed => c,
        _ => computed,
    };
    Learn {
        handle: field.handle.clone(),
        app: field.app.clone(),
        expected: format!("{before_tail}{typed}{after_head}"),
        from: field.caret - k as i32,
        to: caret_after + m as i32,
    }
}

/// Read the region again, compare, and hand what changed to the flow. A
/// field that can no longer be read is logged and forgotten.
async fn learn_from(flow: &Flow, learn: Learn) {
    let now = match learn.handle.read(learn.from, learn.to + LEARN_SLACK).await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("learn: cannot read the field back in {}: {e:#}", learn.app);
            return;
        }
    };
    let pairs = corrections(&learn.expected, &now);
    if pairs.is_empty() {
        tracing::debug!("learn: no corrections in {}", learn.app);
        return;
    }
    let learned = match flow.learn(&pairs, &learn.app) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("learn: {e:#}");
            return;
        }
    };
    for l in &learned {
        tracing::info!(
            "learned {:?} -> {:?} in {} (seen {} times{})",
            l.heard,
            l.written,
            learn.app,
            l.count,
            if l.promoted { ", added to the dictionary" } else { "" }
        );
    }
    let promoted: Vec<String> = learned
        .iter()
        .filter(|l| l.promoted)
        .map(|l| format!("{} -> {}", l.heard, l.written))
        .collect();
    if !promoted.is_empty() {
        let body = format!("Learned: {}", promoted.join(", "));
        if let Err(e) = desktopd::notify::notify("parla", &body).await {
            tracing::debug!("notification failed: {e}");
        }
    }
}

/// What a verified deletion did, for the result line.
fn took_back(verified: Verified, typed: usize) -> String {
    match verified {
        Verified::Exact => format!("took back {typed} characters (verified)"),
        Verified::Fuzzy { deleted } => {
            format!("took back {deleted} characters (verified; {typed} were typed)")
        }
        Verified::Blind => format!("took back {typed} characters (unverified)"),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
