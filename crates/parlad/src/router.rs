//! The router: dictation commits straight to the focused window; commands go
//! through the fast-path grammar first and fall through to the judged path.
//! Both paths end in the same [`Policy`] decision, and what the policy wants
//! confirmed waits in [`Confirmations`] for the next command-mode utterance.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use desktopd::{Command, DesktopIndex, Executor, Window, WindowTarget};
use parla_grammar::{Grammar, Intent};

use crate::command::{from_intent, from_resolved};
use crate::confirm::{check_target, Confirmations, Pending, Taken};
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
}

impl Snapshot {
    pub fn context<'a>(&'a self, index: &'a DesktopIndex) -> Context<'a> {
        Context {
            windows: &self.windows,
            index,
            current_desktop: self.current_desktop,
            desktop_count: self.desktop_count,
            claude_running: self.claude_running,
        }
    }
}

/// What handling an utterance came to. `Done` is a result to report;
/// `Confirm` is a question the user has to answer by voice, so the caller
/// should make sure it is heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handled {
    Done(String),
    Confirm(String),
}

pub struct Router {
    executor: Arc<Executor>,
    grammar: Arc<Grammar>,
    judge: Option<Arc<Judge>>,
    policy: Policy,
    pending: Confirmations,
    /// How long a confirmation prompt stays answerable.
    confirm_window: Duration,
}

impl Router {
    /// The policy comes from the judge's config when there is one, so both
    /// paths share thresholds. Without a judge only grammar matches reach
    /// the gate, and those need no threshold.
    pub fn new(
        executor: Arc<Executor>,
        grammar: Arc<Grammar>,
        judge: Option<Arc<Judge>>,
        confirm_window: Duration,
    ) -> Self {
        let policy = judge
            .as_ref()
            .map_or_else(Policy::default, |j| j.policy().clone());
        Self {
            executor,
            grammar,
            judge,
            policy,
            pending: Confirmations::default(),
            confirm_window,
        }
    }

    /// The thresholds both paths are gated by.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Handle a finished transcript. Returns a short human-readable result
    /// (used for notification/TTS); Err for failures.
    pub async fn handle(&self, mode: Mode, transcript: &str) -> anyhow::Result<Handled> {
        let transcript = transcript.trim();
        anyhow::ensure!(!transcript.is_empty(), "empty transcript");
        match mode {
            // Dictation is not an answer: a pending prompt survives it.
            Mode::Dictate => self.executor.type_text(transcript).await.map(Handled::Done),
            Mode::Command => self.route_command(transcript).await,
        }
    }

    /// Read the state the judged path needs. Public so `parlad --judge` can
    /// use the same observation instead of its own copy.
    pub async fn snapshot(&self) -> anyhow::Result<Snapshot> {
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
        })
    }

    async fn route_command(&self, transcript: &str) -> anyhow::Result<Handled> {
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
        self.route_judged(judge, transcript).await
    }

    async fn route_judged(&self, judge: &Judge, transcript: &str) -> anyhow::Result<Handled> {
        // Observed facts the judgment needs. Gathered here rather than inside
        // the judge so the judge stays a pure function of the state it is
        // given, and so a failure to observe is a router error, not a fact.
        let snapshot = self.snapshot().await?;
        let ctx = snapshot.context(self.executor.desktop_index());

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
            Decision::Act => self.executor.execute(command).await.map(Handled::Done),
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
        Ok(Handled::Confirm(format!("{}? say yes", capitalize(&describe))))
    }

    /// "yes": run the parked command if it is still in time and its window
    /// is still the one the prompt named.
    async fn confirm(&self) -> anyhow::Result<Handled> {
        let pending = match self.pending.take(Instant::now()) {
            Taken::Nothing => anyhow::bail!("nothing to confirm"),
            Taken::Expired(p) => anyhow::bail!(
                "too late to confirm {} (say the command again)",
                p.describe
            ),
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
