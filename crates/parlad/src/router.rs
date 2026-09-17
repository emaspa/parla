//! The router: dictation commits straight to the focused window; commands go
//! through the fast-path grammar first and fall through to the judged path.

use std::sync::Arc;

use desktopd::Executor;
use parla_grammar::Grammar;

use crate::judge::{Context, Judge, Judgment};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Transcript becomes keystrokes into whatever is focused.
    Dictate,
    /// Transcript is parsed as a command.
    Command,
}

pub struct Router {
    executor: Arc<Executor>,
    grammar: Arc<Grammar>,
    judge: Option<Arc<Judge>>,
}

impl Router {
    pub fn new(executor: Arc<Executor>, grammar: Arc<Grammar>, judge: Option<Arc<Judge>>) -> Self {
        Self {
            executor,
            grammar,
            judge,
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

    async fn route_command(&self, transcript: &str) -> anyhow::Result<String> {
        if let Some(intent) = self.grammar.parse(transcript) {
            if intent.needs_confirmation() {
                // P2 wires spoken confirmation; until then refuse loudly
                // rather than execute destructive verbs unconfirmed.
                anyhow::bail!(
                    "intent {intent:?} needs confirmation (spoken-confirm flow lands in P2); not executed"
                );
            }
            tracing::info!("fast path: {intent:?}");
            return self.executor.execute(intent).await;
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
        // given, and so a failure to list windows is a router error.
        let windows = self.executor.list_windows().await.unwrap_or_else(|e| {
            tracing::warn!("window list unavailable for judging: {e:#}");
            Vec::new()
        });
        let (current_desktop, desktop_count) = match desktopd::kwin::list_desktops().await {
            Ok(d) => (
                desktopd::kwin::current_desktop().await.unwrap_or(1),
                d.len() as u32,
            ),
            Err(e) => {
                tracing::warn!("desktop list unavailable for judging: {e:#}");
                (1, 1)
            }
        };
        let ctx = Context {
            windows: &windows,
            index: self.executor.desktop_index(),
            current_desktop,
            desktop_count,
            claude_running: self.executor.tmux().session_exists().await,
        };

        match judge.judge(transcript, &ctx).await? {
            Judgment::Act {
                intent,
                confidence,
                needs_confirmation,
            } => {
                if needs_confirmation {
                    // Same gate as the fast path: voice is an unauthenticated
                    // input channel, so nothing risky runs unconfirmed.
                    anyhow::bail!(
                        "judged {intent:?} at confidence {confidence:.2} needs confirmation \
                         (spoken-confirm flow lands in P2); not executed"
                    );
                }
                tracing::info!("judged path: {intent:?} (confidence {confidence:.2})");
                self.executor.execute(intent).await
            }
            Judgment::Dictation => {
                anyhow::bail!(
                    "that sounded like dictation, not a command — hold the dictate hotkey instead"
                )
            }
            Judgment::Unclear(reason) => {
                anyhow::bail!("could not act on {transcript:?}: {reason}")
            }
        }
    }
}
