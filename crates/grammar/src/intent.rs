use serde::{Deserialize, Serialize};

/// A parsed fast-path command. Everything here must be executable without an
/// LLM round trip; anything that can't be expressed as an `Intent` falls
/// through to the agent path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "intent", content = "args")]
pub enum Intent {
    /// Launch an application by fuzzy-matched .desktop id.
    LaunchApp { query: String },
    /// Open the configured default terminal.
    OpenTerminal,
    /// Focus a window by fuzzy-matched title/app name.
    FocusWindow { query: String },
    /// Close a window (focused one when `query` is None).
    CloseWindow { query: Option<String> },
    /// Minimize the focused (or matched) window.
    MinimizeWindow { query: Option<String> },
    /// Maximize the focused (or matched) window.
    MaximizeWindow { query: Option<String> },
    /// Switch to virtual desktop n (1-based).
    VirtualDesktop { n: u32 },
    /// Switch desktops relative to the current one ("next desktop").
    VirtualDesktopRel { delta: i32 },
    /// Fire an existing KDE global shortcut.
    RunShortcut { component: String, action: String },
    /// KRunner query.
    KRunner { query: String },
    /// Start Claude Code in tmux, optionally with a model.
    StartClaude { model: Option<String> },
    /// Switch the running Claude Code session's model (/model X).
    ClaudeModel { model: String },
    /// Send a prompt to the running Claude Code session.
    ClaudeTell { text: String },
    /// Read back the tail of the Claude Code session.
    ClaudeRead,
    /// Send a desktop notification.
    Notify { text: String },
    /// Send a raw key chord to the focused window (e.g. "ctrl s").
    Key { chord: String },
}

impl Intent {
    /// Destructive intents require spoken confirmation before execution
    /// (plan §5: voice is an unauthenticated input channel).
    pub fn needs_confirmation(&self) -> bool {
        matches!(
            self,
            Intent::CloseWindow { .. } | Intent::RunShortcut { .. }
        )
    }

    /// Construct an intent from a rule's intent name and merged args
    /// (fixed args from config plus the trailing capture slot).
    pub fn from_args(name: &str, args: &std::collections::BTreeMap<String, String>) -> Option<Intent> {
        use crate::normalize::clean_model_name;
        let get = |k: &str| args.get(k).cloned();
        Some(match name {
            "launch_app" => Intent::LaunchApp { query: get("query")? },
            "open_terminal" => Intent::OpenTerminal,
            "focus_window" => Intent::FocusWindow { query: get("query")? },
            "close_window" => Intent::CloseWindow { query: get("query") },
            "minimize_window" => Intent::MinimizeWindow { query: get("query") },
            "maximize_window" => Intent::MaximizeWindow { query: get("query") },
            "virtual_desktop" => Intent::VirtualDesktop { n: get("n")?.parse().ok()? },
            "virtual_desktop_rel" => Intent::VirtualDesktopRel { delta: get("delta")?.parse().ok()? },
            "run_shortcut" => Intent::RunShortcut {
                component: get("component")?,
                action: get("action")?,
            },
            "krunner" => Intent::KRunner { query: get("query")? },
            "start_claude" => Intent::StartClaude { model: get("model").map(|m| clean_model_name(&m)) },
            "claude_model" => Intent::ClaudeModel { model: clean_model_name(&get("model")?) },
            "claude_tell" => Intent::ClaudeTell { text: get("text")? },
            "claude_read" => Intent::ClaudeRead,
            "notify" => Intent::Notify { text: get("text")? },
            "key" => Intent::Key { chord: get("chord")? },
            _ => {
                tracing::warn!("unknown intent name in rule: {name}");
                return None;
            }
        })
    }
}
