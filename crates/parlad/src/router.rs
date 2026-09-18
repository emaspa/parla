//! The router: dictation goes through the flow (snippets, dictionary,
//! cleanup) and is typed into the focused window; commands go through the
//! fast-path grammar first and fall through to the judged path. Both
//! command paths end in the same [`Policy`] decision, and what the policy
//! wants confirmed waits in [`Confirmations`] for the next command-mode
//! utterance.
//!
//! The router also remembers the last dictation for a while, so "scratch
//! that" can take it back and "make that more formal" can rewrite it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use desktopd::{Command, DesktopIndex, Executor, Window, WindowTarget};
use parla_grammar::{Grammar, Intent};

use crate::command::{from_intent, from_resolved};
use crate::confirm::{check_target, Confirmations, Pending, Taken};
use crate::dbus::{Bus, State};
use crate::flow::{Flow, Processed};
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
}

pub struct Router {
    executor: Arc<Executor>,
    grammar: Arc<Grammar>,
    judge: Option<Arc<Judge>>,
    flow: Arc<Flow>,
    policy: Policy,
    pending: Confirmations,
    last: Mutex<Option<LastDictation>>,
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
            last: Mutex::new(None),
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
    /// when the capture started, so dictation is shaped for where it lands.
    pub async fn handle(
        &self,
        mode: Mode,
        transcript: &str,
        focused: Option<&Window>,
    ) -> anyhow::Result<Handled> {
        let transcript = transcript.trim();
        anyhow::ensure!(!transcript.is_empty(), "empty transcript");
        match mode {
            // Dictation is not an answer: a pending prompt survives it.
            Mode::Dictate => self.dictate(transcript, focused).await,
            Mode::Command => self.route_command(transcript, focused).await,
        }
    }

    async fn dictate(&self, transcript: &str, focused: Option<&Window>) -> anyhow::Result<Handled> {
        let class = focused.map(|w| w.class.as_str()).unwrap_or_default();
        self.progress(State::Thinking, Mode::Dictate);
        let processed = self.flow.process(transcript, class).await;
        self.progress(State::Typing, Mode::Dictate);
        self.executor.type_text(&processed.text).await?;
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(LastDictation {
            text: processed.text.clone(),
            window_id: focused.map(|w| w.id.clone()).unwrap_or_default(),
            class: class.to_string(),
            at: Instant::now(),
        });
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
    /// and the focus has not moved, by deleting as many characters as were
    /// typed.
    async fn scratch(&self, focused: Option<&Window>) -> anyhow::Result<Handled> {
        let last = self
            .recent_dictation(focused)
            .ok_or_else(|| anyhow::anyhow!("nothing recent to take back"))?;
        self.progress(State::Typing, Mode::Command);
        let n = last.text.chars().count();
        self.executor.backspace(n).await?;
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(Handled::Done(format!("took back {n} characters")))
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
        self.executor.backspace(last.text.chars().count()).await?;
        self.executor.type_text(&text).await?;
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(LastDictation {
            text: text.clone(),
            at: Instant::now(),
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

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
