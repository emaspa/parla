//! Confirmation policy: the one place that decides whether an intent runs,
//! asks first, or is refused.
//!
//! Both router paths end here. The grammar path brings an intent and nothing
//! else; the judged path brings an intent plus the model's signals. Keeping
//! the thresholds and the rule in one type means a grammar-matched `Key` and
//! a judged `Key` are gated by the same code, and a threshold change is one
//! edit.

use parla_grammar::Intent;

use crate::config::TypeSafeConfig;

/// Thresholds. Copied out of [`TypeSafeConfig`] so the policy can be built
/// and tested without the rest of the config.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Below this intent confidence, act on nothing.
    pub min_confidence: f64,
    /// At or above this, act without a confirmation prompt — unless the
    /// intent always confirms or is judged destructive.
    pub act_unconfirmed_above: f64,
    /// `is_dictation` at or above this means the user held the wrong hotkey.
    pub dictation_threshold: f64,
    /// `is_destructive` at or above this forces spoken confirmation.
    pub destructive_threshold: f64,
}

impl From<&TypeSafeConfig> for Policy {
    fn from(cfg: &TypeSafeConfig) -> Self {
        Self {
            min_confidence: cfg.min_confidence,
            act_unconfirmed_above: cfg.act_unconfirmed_above,
            dictation_threshold: cfg.dictation_threshold,
            destructive_threshold: cfg.destructive_threshold,
        }
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::from(&TypeSafeConfig::default())
    }
}

/// What is known about how an intent was obtained.
#[derive(Debug, Clone, PartialEq)]
pub enum Signals {
    /// A literal grammar match. Exact by construction, and nobody asked the
    /// model anything, so only the intent's own rule applies.
    Grammar,
    /// The judged path. Each `None` is a question the model did not answer
    /// (absent, or answered with the wrong type); the policy treats a missing
    /// safety answer as a reason to ask, never as "safe".
    Judged {
        /// Weakest-link confidence across the intent and its arguments.
        confidence: f64,
        /// Probability that carrying this out destroys work.
        destructive: Option<f64>,
        /// Probability that the utterance was prose, not a command.
        dictation: Option<f64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Act,
    /// Ask the user before executing.
    Confirm { reason: String },
    /// Do not execute, even if confirmed.
    Refuse { reason: String },
}

impl Policy {
    /// Was the utterance judged to be dictation rather than a command?
    pub fn is_prose(&self, dictation: Option<f64>) -> bool {
        dictation.is_some_and(|p| p >= self.dictation_threshold)
    }

    pub fn decide(&self, intent: &Intent, signals: &Signals) -> Decision {
        let confirm = |reason: String| Decision::Confirm { reason };
        match signals {
            Signals::Grammar => {
                if intent.needs_confirmation() {
                    confirm(format!("{} always confirms", intent_name(intent)))
                } else {
                    Decision::Act
                }
            }
            Signals::Judged {
                confidence,
                destructive,
                dictation,
            } => {
                if self.is_prose(*dictation) {
                    return Decision::Refuse {
                        reason: "that sounded like dictation, not a command".into(),
                    };
                }
                if *confidence < self.min_confidence {
                    return Decision::Refuse {
                        reason: format!(
                            "confidence {confidence:.2} is below the {:.2} floor",
                            self.min_confidence
                        ),
                    };
                }
                // Being confident about a destructive request is not
                // permission to carry it out, so risk alone forces the
                // prompt; so does not knowing the risk.
                if intent.needs_confirmation() {
                    return confirm(format!("{} always confirms", intent_name(intent)));
                }
                match destructive {
                    None => return confirm("the model gave no safety judgment".into()),
                    Some(p) if *p >= self.destructive_threshold => {
                        return confirm(format!("judged destructive ({p:.2})"));
                    }
                    Some(_) => {}
                }
                if dictation.is_none() {
                    return confirm("the model did not say whether this was dictation".into());
                }
                if *confidence < self.act_unconfirmed_above {
                    return confirm(format!(
                        "confidence {confidence:.2} is below the {:.2} needed to act unasked",
                        self.act_unconfirmed_above
                    ));
                }
                Decision::Act
            }
        }
    }
}

/// The variant name, for messages that should not dump the arguments.
fn intent_name(intent: &Intent) -> &'static str {
    match intent {
        Intent::LaunchApp { .. } => "launch_app",
        Intent::OpenTerminal => "open_terminal",
        Intent::FocusWindow { .. } => "focus_window",
        Intent::CloseWindow { .. } => "close_window",
        Intent::MinimizeWindow { .. } => "minimize_window",
        Intent::MaximizeWindow { .. } => "maximize_window",
        Intent::VirtualDesktop { .. } => "virtual_desktop",
        Intent::VirtualDesktopRel { .. } => "virtual_desktop_rel",
        Intent::RunShortcut { .. } => "run_shortcut",
        Intent::KRunner { .. } => "krunner",
        Intent::StartClaude { .. } => "start_claude",
        Intent::ClaudeModel { .. } => "claude_model",
        Intent::ClaudeTell { .. } => "claude_tell",
        Intent::ClaudeRead => "claude_read",
        Intent::Notify { .. } => "notify",
        Intent::Key { .. } => "key",
        Intent::Confirm => "confirm",
        Intent::Deny => "deny",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judged(confidence: f64, destructive: Option<f64>, dictation: Option<f64>) -> Signals {
        Signals::Judged {
            confidence,
            destructive,
            dictation,
        }
    }

    fn launch() -> Intent {
        Intent::LaunchApp {
            query: "Firefox".into(),
        }
    }

    #[test]
    fn grammar_key_and_close_confirm_like_the_judged_path() {
        let p = Policy::default();
        for intent in [
            Intent::Key {
                chord: "ctrl s".into(),
            },
            Intent::CloseWindow { query: None },
            Intent::ClaudeTell {
                text: "rm -rf".into(),
            },
            Intent::RunShortcut {
                component: "kwin".into(),
                action: "Kill Window".into(),
            },
        ] {
            assert!(matches!(
                p.decide(&intent, &Signals::Grammar),
                Decision::Confirm { .. }
            ));
            assert!(matches!(
                p.decide(&intent, &judged(1.0, Some(0.0), Some(0.0))),
                Decision::Confirm { .. }
            ));
        }
        assert_eq!(p.decide(&launch(), &Signals::Grammar), Decision::Act);
    }

    #[test]
    fn missing_safety_answers_confirm_rather_than_act() {
        let p = Policy::default();
        assert!(matches!(
            p.decide(&launch(), &judged(0.99, None, Some(0.01))),
            Decision::Confirm { .. }
        ));
        assert!(matches!(
            p.decide(&launch(), &judged(0.99, Some(0.01), None)),
            Decision::Confirm { .. }
        ));
        assert_eq!(
            p.decide(&launch(), &judged(0.99, Some(0.01), Some(0.01))),
            Decision::Act
        );
    }

    #[test]
    fn low_confidence_refuses_and_middling_confidence_asks() {
        let p = Policy::default();
        assert!(matches!(
            p.decide(&launch(), &judged(0.2, Some(0.0), Some(0.0))),
            Decision::Refuse { .. }
        ));
        assert!(matches!(
            p.decide(&launch(), &judged(0.6, Some(0.0), Some(0.0))),
            Decision::Confirm { .. }
        ));
    }

    #[test]
    fn dictation_and_risk_are_decided_here_too() {
        let p = Policy::default();
        assert!(matches!(
            p.decide(&launch(), &judged(0.99, Some(0.0), Some(0.9))),
            Decision::Refuse { .. }
        ));
        assert!(matches!(
            p.decide(
                &Intent::MinimizeWindow { query: None },
                &judged(0.99, Some(0.9), Some(0.0))
            ),
            Decision::Confirm { .. }
        ));
    }
}
