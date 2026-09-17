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

use crate::judge::Resolved;

/// The command a grammar intent asks for, or None for the replies
/// (`Confirm`, `Deny`) that address the router rather than the desktop.
/// Window queries keep the executor's convention that an empty query, or
/// the bare word "window", means the focused window.
pub fn from_intent(intent: Intent) -> Option<Command> {
    let window = |op, query: Option<String>| Command::Window {
        op,
        target: WindowTarget::from_query(query.as_deref()),
    };
    Some(match intent {
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
        Intent::Confirm | Intent::Deny => return None,
    })
}

/// The command a judged intent asks for. Where the judge picked a window or
/// an application from the observed state, the command addresses it by id;
/// the title or name in the intent was only ever a label for that choice.
pub fn from_resolved(resolved: Resolved) -> Option<Command> {
    let Resolved {
        intent,
        window_id,
        entry_id,
        ..
    } = resolved;
    let command = from_intent(intent)?;
    Some(match (command, window_id, entry_id) {
        (Command::Window { op, .. }, Some(id), _) => Command::Window {
            op,
            target: WindowTarget::Id(id),
        },
        (Command::LaunchApp { .. }, _, Some(id)) => Command::LaunchApp {
            app: AppTarget::Entry(id),
        },
        (command, _, _) => command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Signals;

    fn resolved(intent: Intent, window_id: Option<&str>, entry_id: Option<&str>) -> Resolved {
        Resolved {
            intent,
            window_id: window_id.map(String::from),
            entry_id: entry_id.map(String::from),
            confidence: 0.9,
            signals: Signals::Grammar,
        }
    }

    #[test]
    fn judged_targets_go_by_id() {
        let close = Intent::CloseWindow {
            query: Some("build — Konsole".into()),
        };
        assert_eq!(
            from_resolved(resolved(close.clone(), Some("{k1}"), None)),
            Some(Command::Window {
                op: WindowOp::Close,
                target: WindowTarget::Id("{k1}".into())
            })
        );
        // An app chosen from the index launches by its .desktop id, and a
        // window id that the judge did not produce leaves the query alone.
        assert_eq!(
            from_resolved(resolved(
                Intent::LaunchApp {
                    query: "Firefox".into()
                },
                None,
                Some("firefox.desktop")
            )),
            Some(Command::LaunchApp {
                app: AppTarget::Entry("firefox.desktop".into())
            })
        );
        assert_eq!(
            from_resolved(resolved(close, None, None)),
            Some(Command::Window {
                op: WindowOp::Close,
                target: WindowTarget::Query("build — Konsole".into())
            })
        );
    }

    #[test]
    fn window_queries_keep_the_executor_convention() {
        assert_eq!(
            from_intent(Intent::CloseWindow { query: None }),
            Some(Command::Window {
                op: WindowOp::Close,
                target: WindowTarget::Focused
            })
        );
        assert_eq!(
            from_intent(Intent::FocusWindow {
                query: "kate".into()
            }),
            Some(Command::Window {
                op: WindowOp::Focus,
                target: WindowTarget::Query("kate".into())
            })
        );
        assert_eq!(
            from_intent(Intent::LaunchApp {
                query: "firefox".into()
            }),
            Some(Command::LaunchApp {
                app: AppTarget::Query("firefox".into())
            })
        );
        assert_eq!(from_intent(Intent::Confirm), None);
        assert_eq!(from_intent(Intent::Deny), None);
    }
}
