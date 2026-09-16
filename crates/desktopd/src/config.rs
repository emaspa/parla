use serde::{Deserialize, Serialize};

/// Executor configuration. Mirrors the `[desktopd]` section of parla.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopdConfig {
    /// Terminal emulator binary (launched with `terminal_run_args` + command).
    pub terminal: String,
    /// Args that make the terminal run the rest of the command line, e.g. ["-e"].
    pub terminal_run_args: Vec<String>,
    /// tmux session name hosting the persistent Claude Code session (plan §3).
    pub claude_tmux_session: String,
    /// Claude Code command.
    pub claude_command: String,
    /// Injector preference order; first one whose probe succeeds wins.
    /// "eis" (KWin EIS over libei/reis) or "ydotool".
    pub injectors: Vec<String>,
    /// Override the ydotool socket path (default: ydotool's own discovery).
    pub ydotool_socket: Option<String>,
    /// Focus an already-running instance instead of launching a new one.
    pub focus_if_running: bool,
}

impl Default for DesktopdConfig {
    fn default() -> Self {
        Self {
            terminal: "konsole".into(),
            terminal_run_args: vec!["-e".into()],
            claude_tmux_session: "claude-main".into(),
            claude_command: "claude".into(),
            injectors: vec!["eis".into(), "ydotool".into()],
            ydotool_socket: None,
            focus_if_running: true,
        }
    }
}
