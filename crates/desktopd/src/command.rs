//! What the executor can be asked to do. This is the executor's own
//! vocabulary: a caller that parsed speech, judged an utterance, or received
//! an MCP call maps its result onto a `Command`, and the executor never needs
//! to know how the request was phrased.
//!
//! Targets are explicit about how much resolving is left to do. A
//! `WindowTarget::Id` was already picked from the live window list and is
//! acted on as is; a `Query` still has to be matched against titles. The
//! same split holds for applications: an `Entry` names a `.desktop` id the
//! caller chose from the index, a `Query` is a spoken name.

use serde::{Deserialize, Serialize};

use crate::executor::WindowOp;

/// Which window a command acts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowTarget {
    /// A spoken title or application name, matched fuzzily against the
    /// open windows at execution time.
    Query(String),
    /// A KWin window id (`{uuid}`) chosen from a snapshot.
    Id(String),
    /// Whatever has focus when the command runs.
    Focused,
}

impl WindowTarget {
    /// The executor's convention for "no query": empty, or the bare word
    /// "window", means the focused window.
    pub fn from_query(query: Option<&str>) -> Self {
        match query.map(str::trim) {
            Some(q) if !q.is_empty() && q != "window" => WindowTarget::Query(q.to_string()),
            _ => WindowTarget::Focused,
        }
    }
}

/// Which application a command launches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppTarget {
    /// A spoken name, resolved against the `.desktop` index at execution.
    Query(String),
    /// A `.desktop` id such as `org.kde.dolphin.desktop`.
    Entry(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "command", content = "args")]
pub enum Command {
    LaunchApp {
        app: AppTarget,
    },
    /// Open the configured default terminal.
    OpenTerminal,
    /// Focus, close, minimize or maximize one window.
    Window {
        op: WindowOp,
        target: WindowTarget,
    },
    /// Switch to virtual desktop `n` (1-based).
    VirtualDesktop {
        n: u32,
    },
    /// Switch desktops relative to the current one.
    VirtualDesktopRel {
        delta: i32,
    },
    /// Fire an existing KDE global shortcut.
    RunShortcut {
        component: String,
        action: String,
    },
    KRunner {
        query: String,
    },
    /// Start Claude Code in tmux, optionally with a model.
    StartClaude {
        model: Option<String>,
    },
    /// Switch the running Claude Code session's model.
    ClaudeModel {
        model: String,
    },
    /// Send a prompt to the running Claude Code session.
    ClaudeTell {
        text: String,
    },
    /// Read back the tail of the Claude Code session.
    ClaudeRead,
    Notify {
        text: String,
    },
    /// Send a raw key chord to the focused window.
    Key {
        chord: String,
    },
}

impl Command {
    /// The window this command acts on, if it is a window command.
    pub fn window_target(&self) -> Option<&WindowTarget> {
        match self {
            Command::Window { target, .. } => Some(target),
            _ => None,
        }
    }

    /// The same command with its window target replaced. A no-op for
    /// commands that do not act on a window.
    pub fn with_window_target(self, target: WindowTarget) -> Self {
        match self {
            Command::Window { op, .. } => Command::Window { op, target },
            other => other,
        }
    }

    /// A short imperative description ("close 'build — Konsole'"), for a
    /// confirmation prompt. `window_title` names the window when the target
    /// has been resolved; otherwise the target is described as spoken.
    pub fn describe(&self, window_title: Option<&str>) -> String {
        match self {
            Command::LaunchApp { app } => match app {
                AppTarget::Query(q) | AppTarget::Entry(q) => format!("launch {q:?}"),
            },
            Command::OpenTerminal => "open a terminal".into(),
            Command::Window { op, target } => {
                let what = match (window_title, target) {
                    (Some(title), _) => format!("{title:?}"),
                    (None, WindowTarget::Query(q)) => format!("the window matching {q:?}"),
                    (None, WindowTarget::Id(id)) => format!("window {id}"),
                    (None, WindowTarget::Focused) => "the focused window".into(),
                };
                format!("{} {what}", op.verb())
            }
            Command::VirtualDesktop { n } => format!("switch to desktop {n}"),
            Command::VirtualDesktopRel { delta } if *delta < 0 => {
                "go to the previous desktop".into()
            }
            Command::VirtualDesktopRel { .. } => "go to the next desktop".into(),
            Command::RunShortcut { component, action } => {
                format!("run shortcut {component}/{action}")
            }
            Command::KRunner { query } => format!("search for {query:?}"),
            Command::StartClaude { model: Some(m) } => format!("start claude with {m}"),
            Command::StartClaude { model: None } => "start claude".into(),
            Command::ClaudeModel { model } => format!("switch claude to {model}"),
            Command::ClaudeTell { text } => format!("tell claude {text:?}"),
            Command::ClaudeRead => "read claude's output".into(),
            Command::Notify { text } => format!("notify {text:?}"),
            Command::Key { chord } => format!("press {chord}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_window_word_means_focused() {
        assert_eq!(WindowTarget::from_query(None), WindowTarget::Focused);
        assert_eq!(
            WindowTarget::from_query(Some(" window ")),
            WindowTarget::Focused
        );
        assert_eq!(
            WindowTarget::from_query(Some("kate")),
            WindowTarget::Query("kate".into())
        );
    }

    #[test]
    fn describe_prefers_the_resolved_title() {
        let c = Command::Window {
            op: WindowOp::Close,
            target: WindowTarget::Query("konsole".into()),
        };
        assert_eq!(
            c.describe(Some("build — Konsole")),
            "close \"build — Konsole\""
        );
        assert_eq!(c.describe(None), "close the window matching \"konsole\"");
        assert_eq!(
            Command::Key {
                chord: "ctrl s".into()
            }
            .describe(None),
            "press ctrl s"
        );
    }
}
