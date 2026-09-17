//! The one action waiting for a spoken "yes".
//!
//! Voice is an unauthenticated channel, so anything the policy wants
//! confirmed is parked here with a deadline and the id of the window it was
//! described against. Saying yes carries it out only if the deadline has
//! not passed and the target is still the window the prompt named; anything
//! else said in command mode drops it. Time is passed in rather than read,
//! so the machine can be tested at any instant.

use std::sync::Mutex;
use std::time::Instant;

use desktopd::{Command, Window};

/// An action parked behind a confirmation prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub command: Command,
    /// The window the prompt described, when the command acts on one. The
    /// confirmation is only good for this window.
    pub window_id: Option<String>,
    /// Why the policy asked, for the log.
    pub reason: String,
    /// What the prompt said would happen ("close 'build — Konsole'").
    pub describe: String,
    pub deadline: Instant,
}

/// What a "yes" found waiting.
#[derive(Debug, PartialEq, Eq)]
pub enum Taken {
    Nothing,
    Expired(Pending),
    Live(Pending),
}

/// At most one action waits at a time; a new prompt replaces the old one.
#[derive(Debug, Default)]
pub struct Confirmations {
    slot: Mutex<Option<Pending>>,
}

impl Confirmations {
    /// Park `pending`, returning whatever it replaced.
    pub fn arm(&self, pending: Pending) -> Option<Pending> {
        self.lock().replace(pending)
    }

    /// Remove the parked action, sorted by whether `now` is still inside
    /// its window. Expired actions are handed back so the caller can say
    /// what was too late, but they are never live again.
    pub fn take(&self, now: Instant) -> Taken {
        match self.lock().take() {
            None => Taken::Nothing,
            Some(p) if now > p.deadline => Taken::Expired(p),
            Some(p) => Taken::Live(p),
        }
    }

    /// Drop the parked action, returning it for the log.
    pub fn cancel(&self) -> Option<Pending> {
        self.lock().take()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Pending>> {
        // A poisoned slot only means a panic elsewhere while it was held;
        // the Option inside is still consistent.
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The window a confirmed command may touch: the one the prompt named, and
/// only if it is still what the target resolves to now. `current` is that
/// fresh resolution, None when it failed.
pub fn check_target(pending: &Pending, current: Option<&Window>) -> anyhow::Result<()> {
    let Some(expected) = &pending.window_id else {
        return Ok(());
    };
    match current {
        Some(w) if &w.id == expected => Ok(()),
        Some(w) => anyhow::bail!(
            "not done: the prompt was about {}, but that now means '{}'",
            pending.describe,
            w.title
        ),
        None => anyhow::bail!("not done: the window from '{}' is gone", pending.describe),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use desktopd::{WindowOp, WindowTarget};

    use super::*;

    fn window(id: &str, title: &str) -> Window {
        Window {
            id: id.into(),
            title: title.into(),
            class: "konsole".into(),
            resource_name: String::new(),
            desktop: 1,
            active: true,
            minimized: false,
            stacking: 0,
            pid: 0,
            normal: true,
        }
    }

    fn pending(now: Instant, window_id: Option<&str>) -> Pending {
        Pending {
            command: Command::Window {
                op: WindowOp::Close,
                target: WindowTarget::Query("konsole".into()),
            },
            window_id: window_id.map(String::from),
            reason: "close_window always confirms".into(),
            describe: "close 'build — Konsole'".into(),
            deadline: now + Duration::from_secs(8),
        }
    }

    #[test]
    fn a_prompt_is_answerable_until_its_deadline() {
        let now = Instant::now();
        let c = Confirmations::default();
        assert_eq!(c.take(now), Taken::Nothing);

        assert_eq!(c.arm(pending(now, Some("{k1}"))), None);
        assert_eq!(
            c.take(now + Duration::from_secs(7)),
            Taken::Live(pending(now, Some("{k1}")))
        );
        // taking consumes it: a second yes has nothing to act on
        assert_eq!(c.take(now + Duration::from_secs(7)), Taken::Nothing);

        c.arm(pending(now, Some("{k1}")));
        assert_eq!(
            c.take(now + Duration::from_secs(9)),
            Taken::Expired(pending(now, Some("{k1}")))
        );
        assert_eq!(c.take(now), Taken::Nothing);
    }

    #[test]
    fn a_new_prompt_replaces_and_a_command_cancels() {
        let now = Instant::now();
        let c = Confirmations::default();
        c.arm(pending(now, Some("{k1}")));
        let replaced = c.arm(pending(now, Some("{k2}")));
        assert_eq!(replaced.unwrap().window_id.as_deref(), Some("{k1}"));
        assert_eq!(c.cancel().unwrap().window_id.as_deref(), Some("{k2}"));
        assert_eq!(c.cancel(), None);
        assert_eq!(c.take(now), Taken::Nothing);
    }

    #[test]
    fn a_confirmed_action_refuses_a_stale_target() {
        let now = Instant::now();
        let p = pending(now, Some("{k1}"));
        assert!(check_target(&p, Some(&window("{k1}", "build — Konsole"))).is_ok());
        // the query now resolves to another window: the yes was for k1
        let err = check_target(&p, Some(&window("{k2}", "htop — Konsole")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("htop"), "{err}");
        // the window closed on its own
        assert!(check_target(&p, None).is_err());
        // a command without a window has nothing to go stale
        assert!(check_target(&pending(now, None), None).is_ok());
    }
}
