//! Confirmation policy: the one place that decides whether an intent runs,
//! asks first, or is refused.
//!
//! Both router paths end here. The grammar path brings an intent and nothing
//! else; the judged path brings an intent plus the model's signals. Keeping
//! the thresholds and the rule in one type means a grammar-matched `Key` and
//! a judged `Key` are gated by the same code, and a threshold change is one
//! edit.

use parla_grammar::Intent;

use crate::config::{JudgeConfig, Thresholds};

/// Thresholds. Copied out of [`JudgeConfig`], resolved per backend, so the
/// policy can be built and tested without the rest of the config.
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

impl From<Thresholds> for Policy {
    fn from(t: Thresholds) -> Self {
        Self {
            min_confidence: t.min_confidence,
            act_unconfirmed_above: t.act_unconfirmed_above,
            dictation_threshold: t.dictation_threshold,
            destructive_threshold: t.destructive_threshold,
        }
    }
}

impl From<&JudgeConfig> for Policy {
    fn from(cfg: &JudgeConfig) -> Self {
        Self::from(cfg.thresholds())
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::from(&JudgeConfig::default())
    }
}

/// Where an intent's risk comes from. Most intents decide it themselves:
/// focusing a window or switching desktop destroys nothing, and closing a
/// window or sending a key chord asks first whatever a model would say. Only
/// a KRunner query (a run command may do anything) and an edit of dictated
/// text are for the model to judge, so those are the only ones the judge
/// asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Cannot destroy work: the judged path reports probability 0 unasked.
    Never,
    /// [`Intent::needs_confirmation`] asks first, so a judgment is not
    /// needed and not sought.
    AlwaysConfirmed,
    /// The model is asked `is_destructive`.
    Judged,
}

/// The risk rule for an intent by the judge's name for it (`show_app`
/// covers `launch_app` and `focus_window`). Names the judge cannot build
/// are treated as judged, so a new intent asks until it is placed here.
pub fn risk_rule(name: &str) -> Risk {
    match name {
        "show_app" | "launch_app" | "focus_window" | "open_terminal" | "start_claude"
        | "claude_model" | "claude_read" | "virtual_desktop" | "virtual_desktop_rel"
        | "notify" | "minimize_window" | "maximize_window" | "scratch_that" | "confirm"
        | "deny" => Risk::Never,
        "close_window" | "key" | "claude_tell" | "run_shortcut" => Risk::AlwaysConfirmed,
        _ => Risk::Judged,
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
pub(crate) fn intent_name(intent: &Intent) -> &'static str {
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
        Intent::ScratchThat => "scratch_that",
        Intent::EditText { .. } => "edit_text",
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
    fn risk_rule_agrees_with_the_always_confirm_list() {
        // Every variant, so a new intent cannot be added to one list and
        // not the other.
        let samples = [
            Intent::LaunchApp { query: String::new() },
            Intent::OpenTerminal,
            Intent::FocusWindow { query: String::new() },
            Intent::CloseWindow { query: None },
            Intent::MinimizeWindow { query: None },
            Intent::MaximizeWindow { query: None },
            Intent::VirtualDesktop { n: 1 },
            Intent::VirtualDesktopRel { delta: 1 },
            Intent::RunShortcut {
                component: String::new(),
                action: String::new(),
            },
            Intent::KRunner { query: String::new() },
            Intent::StartClaude { model: None },
            Intent::ClaudeModel { model: String::new() },
            Intent::ClaudeTell { text: String::new() },
            Intent::ClaudeRead,
            Intent::Notify { text: String::new() },
            Intent::Key { chord: String::new() },
            Intent::ScratchThat,
            Intent::EditText { instruction: String::new() },
            Intent::Confirm,
            Intent::Deny,
        ];
        for i in &samples {
            let name = intent_name(i);
            assert_eq!(
                risk_rule(name) == Risk::AlwaysConfirmed,
                i.needs_confirmation(),
                "{name}"
            );
        }
        assert_eq!(risk_rule("show_app"), Risk::Never);
        assert_eq!(risk_rule("krunner"), Risk::Judged);
        assert_eq!(risk_rule("edit_text"), Risk::Judged);
        assert_eq!(risk_rule("something_new"), Risk::Judged);
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
