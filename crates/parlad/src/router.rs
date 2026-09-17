//! The router: dictation commits straight to the focused window; commands go
//! through the fast-path grammar first and fall through to the judged path.
//! Both paths end in the same [`Policy`] decision.

// Declared here rather than in main.rs so the module list there stays the
// daemon's; `mod policy;` in main.rs can replace this line later.
#[path = "policy.rs"]
pub mod policy;

use std::sync::Arc;

use anyhow::Context as _;
use desktopd::{DesktopIndex, Executor, Window};
use parla_grammar::{Grammar, Intent};

use crate::judge::{Context, Judge, Verdict};
use policy::{Decision, Policy, Signals};

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

pub struct Router {
    executor: Arc<Executor>,
    grammar: Arc<Grammar>,
    judge: Option<Arc<Judge>>,
    policy: Policy,
}

impl Router {
    /// The policy comes from the judge's config when there is one, so both
    /// paths share thresholds. Without a judge only grammar matches reach
    /// the gate, and those need no threshold.
    pub fn new(executor: Arc<Executor>, grammar: Arc<Grammar>, judge: Option<Arc<Judge>>) -> Self {
        let policy = judge
            .as_ref()
            .map_or_else(Policy::default, |j| j.policy().clone());
        Self {
            executor,
            grammar,
            judge,
            policy,
        }
    }

    /// Handle a finished transcript. Returns a short human-readable result
    /// (used for notification/TTS); Err for failures.
    pub async fn handle(&self, mode: Mode, transcript: &str) -> anyhow::Result<String> {
        let transcript = transcript.trim();
        anyhow::ensure!(!transcript.is_empty(), "empty transcript");
        match mode {
            Mode::Dictate => self.executor.type_text(transcript).await,
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

    async fn route_command(&self, transcript: &str) -> anyhow::Result<String> {
        if let Some(intent) = self.grammar.parse(transcript) {
            self.gate("fast path", &intent, &Signals::Grammar)?;
            tracing::info!("fast path: {intent:?}");
            return self
                .executor
                .execute(crate::command::from_intent(intent))
                .await;
        }

        // Grammar matches literal word sequences, so anything phrased outside
        // its ~40 patterns lands here.
        let Some(judge) = &self.judge else {
            anyhow::bail!("no fast-path match for {transcript:?} (judged path disabled)");
        };
        self.route_judged(judge, transcript).await
    }

    async fn route_judged(&self, judge: &Judge, transcript: &str) -> anyhow::Result<String> {
        // Observed facts the judgment needs. Gathered here rather than inside
        // the judge so the judge stays a pure function of the state it is
        // given, and so a failure to observe is a router error, not a fact.
        let snapshot = self.snapshot().await?;
        let ctx = snapshot.context(self.executor.desktop_index());

        match judge.judge_verdict(transcript, &ctx).await? {
            Verdict::Act(resolved) => {
                self.gate(
                    &format!("judged (confidence {:.2})", resolved.confidence),
                    &resolved.intent,
                    &resolved.signals,
                )?;
                tracing::info!(
                    "judged path: {:?} (confidence {:.2}, window {:?})",
                    resolved.intent,
                    resolved.confidence,
                    resolved.window_id
                );
                self.executor
                    .execute(crate::command::from_intent(resolved.intent))
                    .await
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

    /// The single confirmation gate. Voice is an unauthenticated input
    /// channel, so nothing the policy wants confirmed runs unconfirmed; until
    /// the spoken-confirm flow lands (P2) that means refusing loudly.
    fn gate(&self, source: &str, intent: &Intent, signals: &Signals) -> anyhow::Result<()> {
        match self.policy.decide(intent, signals) {
            Decision::Act => Ok(()),
            Decision::Confirm { reason } => anyhow::bail!(
                "{source} {intent:?} needs confirmation ({reason}); \
                 spoken-confirm flow lands in P2, not executed"
            ),
            Decision::Refuse { reason } => anyhow::bail!("{source} {intent:?} refused: {reason}"),
        }
    }
}
