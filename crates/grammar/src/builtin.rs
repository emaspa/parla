use crate::pattern::RuleDef;

/// Max capture words for app/window verbs. Long utterances are prose, not
/// commands — they must fall through to the agent path.
const APP_CAP: usize = 6;
const SEARCH_CAP: usize = 8;

/// (pattern, intent, max capture words)
type RuleRow = (&'static str, &'static str, Option<usize>);

/// Default fast-path grammar (plan §1: ~30 verbs, no LLM). Order is
/// priority: earlier rules win, so specific phrases ("start claude") must
/// precede generic captures ("start {query}").
const RULES: &[RuleRow] = &[
    // terminal
    ("open terminal", "open_terminal", None),
    ("open konsole", "open_terminal", None),
    // Claude Code over tmux (plan §3) — before generic "start {query}"
    ("start claude with {model}", "start_claude", None),
    ("start claude code with {model}", "start_claude", None),
    ("start claude", "start_claude", None),
    ("claude model {model}", "claude_model", None),
    ("switch claude to {model}", "claude_model", None),
    ("switch claude model to {model}", "claude_model", None),
    ("set claude model to {model}", "claude_model", None),
    ("tell claude {text}", "claude_tell", None),
    ("ask claude {text}", "claude_tell", None),
    ("tell claude code {text}", "claude_tell", None),
    ("what did claude say", "claude_read", None),
    ("read claude", "claude_read", None),
    ("claude status", "claude_read", None),
    // virtual desktops
    ("next desktop", "virtual_desktop_rel", None),
    ("previous desktop", "virtual_desktop_rel", None),
    ("desktop {n}", "virtual_desktop", None),
    ("go to desktop {n}", "virtual_desktop", None),
    ("switch to desktop {n}", "virtual_desktop", None),
    ("virtual desktop {n}", "virtual_desktop", None),
    // window ops — bare "window" forms before capture forms
    ("close window", "close_window", None),
    ("close the window", "close_window", None),
    ("minimize window", "minimize_window", None),
    ("maximize window", "maximize_window", None),
    ("close {query}", "close_window", Some(APP_CAP)),
    ("minimize {query}", "minimize_window", Some(APP_CAP)),
    ("maximize {query}", "maximize_window", Some(APP_CAP)),
    // window focus — after "switch to desktop {n}"
    ("focus {query}", "focus_window", Some(APP_CAP)),
    ("switch to {query}", "focus_window", Some(APP_CAP)),
    // app launch — after all specific "start/open ..." rules
    ("launch {query}", "launch_app", Some(APP_CAP)),
    ("open {query}", "launch_app", Some(APP_CAP)),
    ("start {query}", "launch_app", Some(APP_CAP)),
    // krunner
    ("search for {query}", "krunner", Some(SEARCH_CAP)),
    ("search {query}", "krunner", Some(SEARCH_CAP)),
    ("find {query}", "krunner", Some(SEARCH_CAP)),
    // misc
    ("notify {text}", "notify", None),
    ("press {chord}", "key", Some(3)),
    ("key {chord}", "key", Some(3)),
];

pub fn builtin_rules() -> Vec<RuleDef> {
    let mut rules: Vec<RuleDef> = RULES
        .iter()
        .map(|(pattern, intent, max)| RuleDef {
            pattern: (*pattern).to_string(),
            intent: (*intent).to_string(),
            args: Default::default(),
            max_capture_words: *max,
        })
        .collect();

    // relative desktop deltas need fixed args
    if let Some(rule) = rules.iter_mut().find(|r| r.pattern == "next desktop") {
        rule.args.insert("delta".into(), "1".into());
    }
    if let Some(rule) = rules.iter_mut().find(|r| r.pattern == "previous desktop") {
        rule.args.insert("delta".into(), "-1".into());
    }

    rules
}
