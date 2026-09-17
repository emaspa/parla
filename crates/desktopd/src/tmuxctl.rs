//! Claude Code control over tmux (plan §3). Never inject keystrokes into a
//! Konsole window to drive Claude Code — drive the tmux session directly and
//! attach a terminal only for viewing.

use crate::proc::{Cmd, ProcError};

pub struct TmuxCtl {
    pub session: String,
    pub claude_command: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartOutcome {
    /// Session did not exist and was created.
    Created,
    /// Session already existed; nothing started.
    AlreadyRunning,
}

impl TmuxCtl {
    pub fn new(session: String, claude_command: String) -> Self {
        Self {
            session,
            claude_command,
        }
    }

    fn tmux(&self) -> Cmd {
        Cmd::new("tmux")
    }

    /// Does the session exist? `Ok(false)` means tmux answered "no";
    /// `Err` means tmux itself could not answer (not installed, timed out).
    pub async fn session_state(&self) -> anyhow::Result<bool> {
        // tmux answers "no" with exit 1, which the helper reports as a
        // failure; only spawn errors and timeouts are real failures here.
        match self
            .tmux()
            .args(["has-session", "-t", &self.session])
            .output()
            .await
        {
            Ok(_) => Ok(true),
            Err(ProcError::Failed { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// `session_state` collapsed to a bool for callers that only display it;
    /// a broken tmux is logged and reads as "not running".
    pub async fn session_exists(&self) -> bool {
        self.session_state().await.unwrap_or_else(|e| {
            tracing::warn!("cannot query tmux: {e:#}");
            false
        })
    }

    /// Idempotent: create the detached Claude session if missing.
    pub async fn ensure_claude_session(&self, model: Option<&str>) -> anyhow::Result<StartOutcome> {
        if self.session_state().await? {
            return Ok(StartOutcome::AlreadyRunning);
        }
        let shell_cmd = match model {
            Some(m) => format!("{} --model {}", self.claude_command, shell_quote(m)),
            None => self.claude_command.clone(),
        };
        self.tmux()
            .args(["new-session", "-d", "-s", &self.session, &shell_cmd])
            .run()
            .await?;
        // give claude a moment to boot before anyone sends keys
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Ok(StartOutcome::Created)
    }

    /// The command running in the session's active pane ("claude", "fish").
    pub async fn pane_command(&self) -> anyhow::Result<String> {
        let out = self
            .tmux()
            .args(["display", "-p", "-t", &self.session, "#{pane_current_command}"])
            .run()
            .await?;
        Ok(out.trim().to_string())
    }

    /// Refuse to type into a pane unless Claude Code is what is reading it.
    /// Keystrokes meant for Claude landing in a shell would run as commands.
    async fn ensure_claude_in_pane(&self) -> anyhow::Result<()> {
        let running = self.pane_command().await?;
        let expected = self
            .claude_command
            .split_whitespace()
            .next()
            .map(|p| p.rsplit('/').next().unwrap_or(p))
            .unwrap_or("claude");
        anyhow::ensure!(
            running == expected || running == "claude",
            "tmux session {:?} is running {running:?}, not {expected:?}; refusing to send",
            self.session
        );
        Ok(())
    }

    /// Send a slash-style command or prompt: literal text, then Enter.
    pub async fn send(&self, text: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.session_state().await?,
            "tmux session {:?} does not exist (start claude first)",
            self.session
        );
        self.ensure_claude_in_pane().await?;
        self.tmux()
            .args(["send-keys", "-t", &self.session, "-l", "--", text])
            .run()
            .await?;
        self.tmux()
            .args(["send-keys", "-t", &self.session, "Enter"])
            .run()
            .await?;
        Ok(())
    }

    pub async fn switch_model(&self, model: &str) -> anyhow::Result<()> {
        self.send(&format!("/model {model}")).await
    }

    /// Capture the visible pane tail for readback/summarization.
    pub async fn read_tail(&self, max_lines: usize) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.session_state().await?,
            "tmux session {:?} does not exist",
            self.session
        );
        let text = self
            .tmux()
            .args(["capture-pane", "-p", "-t", &self.session])
            .run()
            .await?;
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        Ok(lines[start..].join("\n"))
    }

    pub fn attach_command(&self) -> Vec<String> {
        vec!["tmux".into(), "attach".into(), "-t".into(), self.session.clone()]
    }
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
