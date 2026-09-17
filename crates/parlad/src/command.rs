//! From what was understood to what gets done: the mapping from the
//! grammar's [`Intent`] onto the executor's [`Command`].
//!
//! The two vocabularies differ on purpose. An intent says what the user
//! asked for in the words they used ("close firefox"); a command says what
//! the executor will touch, and once the judged path has picked a window
//! from the live list, the command carries that window's id instead of a
//! title to be fuzzy-matched again.

use desktopd::{AppTarget, Command, WindowOp, WindowTarget};
use parla_grammar::Intent;

/// The command a grammar intent asks for. Window queries keep the executor's
/// convention that an empty query, or the bare word "window", means the
/// focused window.
pub fn from_intent(intent: Intent) -> Command {
    let window = |op, query: Option<String>| Command::Window {
        op,
        target: WindowTarget::from_query(query.as_deref()),
    };
    match intent {
        Intent::LaunchApp { query } => Command::LaunchApp {
            app: AppTarget::Query(query),
        },
        Intent::OpenTerminal => Command::OpenTerminal,
        Intent::FocusWindow { query } => window(WindowOp::Focus, Some(query)),
        Intent::CloseWindow { query } => window(WindowOp::Close, query),
        Intent::MinimizeWindow { query } => window(WindowOp::Minimize, query),
        Intent::MaximizeWindow { query } => window(WindowOp::Maximize, query),
        Intent::VirtualDesktop { n } => Command::VirtualDesktop { n },
        Intent::VirtualDesktopRel { delta } => Command::VirtualDesktopRel { delta },
        Intent::RunShortcut { component, action } => Command::RunShortcut { component, action },
        Intent::KRunner { query } => Command::KRunner { query },
        Intent::StartClaude { model } => Command::StartClaude { model },
        Intent::ClaudeModel { model } => Command::ClaudeModel { model },
        Intent::ClaudeTell { text } => Command::ClaudeTell { text },
        Intent::ClaudeRead => Command::ClaudeRead,
        Intent::Notify { text } => Command::Notify { text },
        Intent::Key { chord } => Command::Key { chord },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_queries_keep_the_executor_convention() {
        assert_eq!(
            from_intent(Intent::CloseWindow { query: None }),
            Command::Window {
                op: WindowOp::Close,
                target: WindowTarget::Focused
            }
        );
        assert_eq!(
            from_intent(Intent::FocusWindow {
                query: "kate".into()
            }),
            Command::Window {
                op: WindowOp::Focus,
                target: WindowTarget::Query("kate".into())
            }
        );
        assert_eq!(
            from_intent(Intent::LaunchApp {
                query: "firefox".into()
            }),
            Command::LaunchApp {
                app: AppTarget::Query("firefox".into())
            }
        );
    }
}
