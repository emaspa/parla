//! parla-grammar: the fast path. Turns a spoken utterance into an [`Intent`]
//! without any LLM round trip (plan §1). Custom rules come from TOML so new
//! phrases are data, not code.

pub mod builtin;
pub mod intent;
pub mod normalize;
pub mod pattern;

pub use intent::Intent;
pub use pattern::RuleDef;

use pattern::CompiledRule;

/// Compiled grammar: ordered rules, first match wins.
pub struct Grammar {
    rules: Vec<CompiledRule>,
}

/// TOML shape of a custom grammar file:
/// ```toml
/// [[rule]]
/// pattern = "take a screenshot"
/// intent = "run_shortcut"
/// args = { component = "org_kde_spectacle_desktop", action = "ActiveWindowScreenShot" }
/// ```
#[derive(serde::Deserialize)]
struct GrammarFile {
    #[serde(default)]
    rule: Vec<RuleDef>,
}

impl Grammar {
    pub fn builtin() -> Self {
        Self::compile(builtin::builtin_rules(), Vec::new())
    }

    /// Custom rules take priority over builtins (prepended).
    pub fn with_custom(custom: Vec<RuleDef>) -> Self {
        Self::compile(builtin::builtin_rules(), custom)
    }

    /// Load custom rules from a TOML string, merged over the builtins.
    pub fn from_toml_str(toml_str: &str) -> anyhow::Result<Self> {
        let file: GrammarFile = toml::from_str(toml_str)?;
        Ok(Self::with_custom(file.rule))
    }

    fn compile(builtins: Vec<RuleDef>, custom: Vec<RuleDef>) -> Self {
        let mut rules = Vec::new();
        for def in custom.into_iter().chain(builtins) {
            match CompiledRule::compile(&def) {
                Some(c) => rules.push(c),
                None => tracing::warn!("skipping invalid rule: {:?}", def.pattern),
            }
        }
        Self { rules }
    }

    /// Parse a raw utterance (whisper transcript) into an intent.
    /// Returns None when nothing matches — that's the agent-path fallthrough.
    pub fn parse(&self, utterance: &str) -> Option<Intent> {
        let mapped = normalize::words_with_spans(utterance);
        let words: Vec<&str> = mapped.iter().map(|word| word.normalized.as_str()).collect();
        if words.is_empty() {
            return None;
        }
        for rule in &self.rules {
            if let Some(capture) = rule.match_utterance(&words) {
                let raw_capture = capture.map(|_| {
                    let start = if rule.prefix.is_empty() {
                        0
                    } else {
                        let end = mapped[rule.prefix.len() - 1].raw.end;
                        if utterance[end..].starts_with(char::is_whitespace) {
                            end
                        } else {
                            mapped[rule.prefix.len()].raw.start
                        }
                    };
                    utterance[start..].trim()
                });
                if let Some(intent) = rule.build_intent_from_raw(raw_capture) {
                    tracing::debug!("{:?} -> {:?}", utterance, intent);
                    return Some(intent);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Option<Intent> {
        Grammar::builtin().parse(s)
    }

    #[test]
    fn launch_and_focus() {
        assert_eq!(
            parse("Open Firefox."),
            Some(Intent::LaunchApp {
                query: "Firefox.".into()
            })
        );
        assert_eq!(
            parse("focus kate"),
            Some(Intent::FocusWindow {
                query: "kate".into()
            })
        );
        assert_eq!(parse("open terminal"), Some(Intent::OpenTerminal));
    }

    #[test]
    fn desktops() {
        assert_eq!(parse("desktop two"), Some(Intent::VirtualDesktop { n: 2 }));
        assert_eq!(
            parse("go to desktop 4"),
            Some(Intent::VirtualDesktop { n: 4 })
        );
        assert_eq!(
            parse("next desktop"),
            Some(Intent::VirtualDesktopRel { delta: 1 })
        );
        assert_eq!(
            parse("previous desktop"),
            Some(Intent::VirtualDesktopRel { delta: -1 })
        );
        // "switch to desktop 3" must NOT be focus_window
        assert_eq!(
            parse("switch to desktop 3"),
            Some(Intent::VirtualDesktop { n: 3 })
        );
        assert_eq!(
            parse("switch to firefox"),
            Some(Intent::FocusWindow {
                query: "firefox".into()
            })
        );
    }

    #[test]
    fn claude_code_control() {
        assert_eq!(
            parse("start claude"),
            Some(Intent::StartClaude { model: None })
        );
        assert_eq!(
            parse("start claude with opus"),
            Some(Intent::StartClaude {
                model: Some("opus".into())
            })
        );
        assert_eq!(
            parse("switch Claude to Haiku"),
            Some(Intent::ClaudeModel {
                model: "haiku".into()
            })
        );
        assert_eq!(
            parse("tell claude to fix the failing test in openxlr"),
            Some(Intent::ClaudeTell {
                text: "to fix the failing test in openxlr".into()
            })
        );
        assert_eq!(parse("what did claude say"), Some(Intent::ClaudeRead));
        // generic "start {query}" must not eat "start claude"
        assert_eq!(
            parse("start firefox"),
            Some(Intent::LaunchApp {
                query: "firefox".into()
            })
        );
    }

    #[test]
    fn windows() {
        assert_eq!(
            parse("close window"),
            Some(Intent::CloseWindow { query: None })
        );
        assert_eq!(
            parse("close firefox"),
            Some(Intent::CloseWindow {
                query: Some("firefox".into())
            })
        );
        assert!(parse("close window").unwrap().needs_confirmation());
    }

    #[test]
    fn fallthrough_is_none() {
        assert_eq!(parse("open my email and find the message from alan"), None);
        assert_eq!(parse("what's the weather"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn custom_rules_win() {
        let g = Grammar::from_toml_str(
            r#"
[[rule]]
pattern = "take a screenshot"
intent = "run_shortcut"
args = { component = "org_kde_spectacle_desktop", action = "ActiveWindowScreenShot" }

[[rule]]
pattern = "open the pod bay doors {text}"
intent = "notify"
args = {}
"#,
        )
        .unwrap();
        assert_eq!(
            g.parse("take a screenshot"),
            Some(Intent::RunShortcut {
                component: "org_kde_spectacle_desktop".into(),
                action: "ActiveWindowScreenShot".into()
            })
        );
        assert_eq!(
            g.parse("open the pod bay doors hal"),
            Some(Intent::Notify { text: "hal".into() })
        );
        // custom capture mapping uses slot name as arg key
        let g2 = Grammar::from_toml_str(
            r#"
[[rule]]
pattern = "remind me {text}"
intent = "notify"
"#,
        )
        .unwrap();
        assert_eq!(
            g2.parse("remind me to call mom"),
            Some(Intent::Notify {
                text: "to call mom".into()
            })
        );
    }

    #[test]
    fn every_builtin_rule_is_reachable() {
        let definitions = builtin::builtin_rules();
        // Give each rule a distinct result, so aliases cannot hide shadowing.
        // Use a nonnumeric intent to probe even {n} patterns with literal "x".
        let marked = definitions
            .iter()
            .cloned()
            .map(|mut rule| {
                rule.intent = "run_shortcut".into();
                rule.args.insert("component".into(), rule.pattern.clone());
                rule.args.insert("action".into(), "probe".into());
                rule
            })
            .collect();
        let grammar = Grammar::compile(marked, Vec::new());
        for definition in definitions {
            let compiled = CompiledRule::compile(&definition).unwrap();
            let mut utterance = compiled.prefix.join(" ");
            if compiled.slot.is_some() {
                utterance.push_str(" x");
            }
            assert_eq!(
                grammar.parse(&utterance),
                Some(Intent::RunShortcut {
                    component: definition.pattern.clone(),
                    action: "probe".into(),
                }),
                "unreachable rule: {}",
                definition.pattern
            );
        }
    }
}
