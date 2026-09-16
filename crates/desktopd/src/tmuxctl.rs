//! Claude Code control over tmux (plan §3). Never inject keystrokes into a
//! Konsole window to drive Claude Code — drive the tmux session directly and
//! attach a terminal only for viewing.

use tokio::process::Command;

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

    pub async fn session_exists(&self) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", &self.session])
            .output()
            .await
            .is_ok_and(|o| o.status.success())
    }

    /// Idempotent: create the detached Claude session if missing.
    pub async fn ensure_claude_session(&self, model: Option<&str>) -> anyhow::Result<StartOutcome> {
        if self.session_exists().await {
            return Ok(StartOutcome::AlreadyRunning);
        }
        let shell_cmd = match model {
            Some(m) => format!("{} --model {}", self.claude_command, shell_quote(m)),
            None => self.claude_command.clone(),
        };
        let out = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &self.session,
                &shell_cmd,
            ])
            .output()
            .await?;
        anyhow::ensure!(
            out.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        // give claude a moment to boot before anyone sends keys
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Ok(StartOutcome::Created)
    }

    /// Send a slash-style command or prompt: literal text, then Enter.
    pub async fn send(&self, text: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.session_exists().await,
            "tmux session {:?} does not exist (start claude first)",
            self.session
        );
        let out = Command::new("tmux")
            .args(["send-keys", "-t", &self.session, "-l", "--", text])
            .output()
            .await?;
        anyhow::ensure!(out.status.success(), "tmux send-keys failed");
        let out = Command::new("tmux")
            .args(["send-keys", "-t", &self.session, "Enter"])
            .output()
            .await?;
        anyhow::ensure!(out.status.success(), "tmux send-keys Enter failed");
        Ok(())
    }

    pub async fn switch_model(&self, model: &str) -> anyhow::Result<()> {
        self.send(&format!("/model {model}")).await
    }

    /// Capture the visible pane tail for readback/summarization.
    pub async fn read_tail(&self, max_lines: usize) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.session_exists().await,
            "tmux session {:?} does not exist",
            self.session
        );
        let out = Command::new("tmux")
            .args(["capture-pane", "-p", "-t", &self.session])
            .output()
            .await?;
        anyhow::ensure!(out.status.success(), "tmux capture-pane failed");
        let text = String::from_utf8_lossy(&out.stdout);
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
