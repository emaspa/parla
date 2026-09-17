use parla_grammar::{Grammar, Intent};

#[test]
fn captures_preserve_raw_payloads() {
    let grammar = Grammar::builtin();
    for payload in [
        "to run cargo test --release on ~/.config/parla.toml",
        "--Release ~/.config/Parla.toml",
        "\"Hello, Élodie!\"  One + TWO = Three.",
        "~/Documents/My_File.rs",
    ] {
        for (prefix, expected) in [
            (
                "Tell Claude",
                Intent::ClaudeTell {
                    text: payload.into(),
                },
            ),
            (
                "Notify",
                Intent::Notify {
                    text: payload.into(),
                },
            ),
        ] {
            assert_eq!(
                grammar.parse(&format!("{prefix} {payload}")),
                Some(expected)
            );
        }
    }
    assert_eq!(
        grammar.parse("search for Rust --release ~/.config/Parla.toml"),
        Some(Intent::KRunner {
            query: "Rust --release ~/.config/Parla.toml".into()
        })
    );
    assert_eq!(
        grammar.parse("open One Password"),
        Some(Intent::LaunchApp {
            query: "One Password".into()
        })
    );
    assert_eq!(
        grammar.parse("launch My.App --Profile ~/.Config"),
        Some(Intent::LaunchApp {
            query: "My.App --Profile ~/.Config".into()
        })
    );
    assert_eq!(
        grammar.parse("notify 🚀 Hello!"),
        Some(Intent::Notify {
            text: "🚀 Hello!".into()
        })
    );
    assert_eq!(
        grammar.parse("notify:Hello!"),
        Some(Intent::Notify {
            text: "Hello!".into()
        })
    );
    assert_eq!(
        grammar.parse("desktop Twelve."),
        Some(Intent::VirtualDesktop { n: 12 })
    );
}

#[test]
fn specific_commands_win() {
    let grammar = Grammar::builtin();
    assert_eq!(
        grammar.parse("tell claude code fix it"),
        Some(Intent::ClaudeTell {
            text: "fix it".into()
        })
    );
    assert_eq!(
        grammar.parse("start claude code"),
        Some(Intent::StartClaude { model: None })
    );
    assert_eq!(
        grammar.parse("start claude code with Claude Haiku."),
        Some(Intent::StartClaude {
            model: Some("haiku".into())
        })
    );
    assert_eq!(
        grammar.parse("claude model One."),
        Some(Intent::ClaudeModel {
            model: "one".into()
        })
    );
    assert_eq!(
        grammar.parse("minimize the window"),
        Some(Intent::MinimizeWindow { query: None })
    );
    assert_eq!(
        grammar.parse("maximize the window"),
        Some(Intent::MaximizeWindow { query: None })
    );
}

#[test]
fn key_chords_are_normalized() {
    let grammar = Grammar::builtin();
    assert_eq!(
        grammar.parse("press Ctrl+Shift+S"),
        Some(Intent::Key {
            chord: "ctrl shift s".into()
        })
    );
    assert_eq!(
        grammar.parse("key ALT Tab."),
        Some(Intent::Key {
            chord: "alt tab".into()
        })
    );
}
