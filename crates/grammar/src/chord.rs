//! Spoken key chords: "control shift t" to `ctrl+shift+t`.
//!
//! A chord spoken to a voice assistant comes back from the transcriber as
//! words: modifiers by their names, letters as letters, "f five" for F5,
//! "page down" for Page Down. This maps every word onto the key names the
//! injectors know, and refuses the whole chord when any word is not a key,
//! so "hit the any key" does not become a keystroke.

use crate::normalize::normalize;

/// Words that may surround a chord without being part of it: "press the
/// escape key", "control plus s". Each is filler only when a key word
/// follows it: at the end of the chord "a" is the letter and "plus" the
/// plus key, so "control a" is ctrl+a and "control plus" zooms in.
const FILLER: &[&str] = &["a", "an", "the", "plus", "and", "then"];
/// Keys that only qualify another key. A chord of modifiers alone
/// ("control") presses nothing and is refused.
const MODIFIERS: &[&str] = &["ctrl", "shift", "alt", "altgr", "meta"];
/// Trailing nouns that name the act, not a key: "an escape keystroke".
const TRAILING: &[&str] = &["key", "keys", "keystroke", "button", "chord", "combination"];

/// A spoken word and the key name it stands for. Digits and single
/// letters are handled in code. Punctuation keys are named by their
/// character, which is what both injectors accept; "plus" is the equals
/// key, where + sits on a US layout, so "control plus" is ctrl+= as the
/// zoom-in binding wants.
const KEYS: &[(&str, &str)] = &[
    ("control", "ctrl"),
    ("ctrl", "ctrl"),
    ("ctl", "ctrl"),
    ("shift", "shift"),
    ("alt", "alt"),
    ("option", "alt"),
    ("altgr", "altgr"),
    ("meta", "meta"),
    ("super", "meta"),
    ("win", "meta"),
    ("windows", "meta"),
    ("command", "meta"),
    ("cmd", "meta"),
    ("enter", "enter"),
    ("return", "enter"),
    ("escape", "escape"),
    ("esc", "escape"),
    ("tab", "tab"),
    ("space", "space"),
    ("spacebar", "space"),
    ("backspace", "backspace"),
    ("delete", "delete"),
    ("del", "delete"),
    ("insert", "insert"),
    ("home", "home"),
    ("end", "end"),
    ("pageup", "pageup"),
    ("pagedown", "pagedown"),
    ("up", "up"),
    ("down", "down"),
    ("left", "left"),
    ("right", "right"),
    ("minus", "-"),
    ("dash", "-"),
    ("hyphen", "-"),
    ("equals", "="),
    ("equal", "="),
    ("plus", "="),
    ("comma", ","),
    ("period", "."),
    ("dot", "."),
    ("slash", "/"),
    ("semicolon", ";"),
    ("apostrophe", "'"),
    ("grave", "`"),
    ("backtick", "`"),
    ("backslash", "\\"),
];

/// Number words, for "f five" and "control one".
const DIGITS: &[(&str, &str)] = &[
    ("zero", "0"),
    ("one", "1"),
    ("two", "2"),
    ("three", "3"),
    ("four", "4"),
    ("five", "5"),
    ("six", "6"),
    ("seven", "7"),
    ("eight", "8"),
    ("nine", "9"),
    ("ten", "10"),
    ("eleven", "11"),
    ("twelve", "12"),
];

fn digit(word: &str) -> Option<&'static str> {
    DIGITS.iter().find(|(w, _)| *w == word).map(|(_, d)| *d)
}

/// Every key name `spoken_chord` can emit, for the injectors to check
/// their tables against.
pub fn key_names() -> Vec<String> {
    let mut names: Vec<String> = KEYS.iter().map(|(_, k)| (*k).to_string()).collect();
    names.extend((1..=12).map(|n| format!("f{n}")));
    names.extend(('0'..='9').chain('a'..='z').map(|c| c.to_string()));
    names.sort();
    names.dedup();
    names
}

/// The key name for one spoken word, if it is a key at all.
fn key(word: &str) -> Option<String> {
    if let Some((_, k)) = KEYS.iter().find(|(w, _)| *w == word) {
        return Some((*k).to_string());
    }
    if let Some(rest) = word.strip_prefix('f') {
        let n: Option<u32> = digit(rest)
            .and_then(|d| d.parse().ok())
            .or_else(|| rest.parse().ok());
        if let Some(n) = n.filter(|n| (1..=12).contains(n)) {
            return Some(format!("f{n}"));
        }
    }
    if let Some(d) = digit(word) {
        if d.len() == 1 {
            return Some(d.to_string());
        }
    }
    let mut chars = word.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_string()),
        _ => None,
    }
}

/// The chord spoken in `text`, as `ctrl+shift+t`, or None when any word is
/// not a key or no word is more than a modifier. Filler words around the
/// keys are ignored; "page down" and "f five" are read as one key each;
/// modifiers come first whatever the spoken order.
pub fn spoken_chord(text: &str) -> Option<String> {
    let normalized = normalize(text);
    let mut words: Vec<&str> = normalized.split(' ').filter(|w| !w.is_empty()).collect();
    while words.last().is_some_and(|w| TRAILING.contains(w)) {
        words.pop();
    }
    let mut keys = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        let next = words.get(i + 1).copied();
        // "the" in "the escape key" is filler; "a" in "control a" is a key.
        if FILLER.contains(&w) && words[i + 1..].iter().any(|w| key(w).is_some()) {
            i += 1;
            continue;
        }
        // Two-word keys.
        match (w, next) {
            ("page", Some("up")) => {
                keys.push("pageup".to_string());
                i += 2;
                continue;
            }
            ("page", Some("down")) => {
                keys.push("pagedown".to_string());
                i += 2;
                continue;
            }
            ("f", Some(n)) if n.len() <= 6 => {
                if let Some(k) = key(&format!("f{n}")) {
                    if k.starts_with('f') && k.len() > 1 {
                        keys.push(k);
                        i += 2;
                        continue;
                    }
                }
            }
            (_, Some("arrow")) | (_, Some("key"))
                if matches!(w, "up" | "down" | "left" | "right") =>
            {
                keys.push(w.to_string());
                i += 2;
                continue;
            }
            _ => {}
        }
        keys.push(key(w)?);
        i += 1;
    }
    let is_modifier = |k: &String| MODIFIERS.contains(&k.as_str());
    if !keys.iter().any(|k| !is_modifier(k)) {
        return None;
    }
    let (modifiers, rest): (Vec<String>, Vec<String>) = keys.into_iter().partition(is_modifier);
    Some([modifiers, rest].concat().join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_and_letters() {
        assert_eq!(spoken_chord("control s").as_deref(), Some("ctrl+s"));
        assert_eq!(spoken_chord("Control+S").as_deref(), Some("ctrl+s"));
        assert_eq!(spoken_chord("alt tab").as_deref(), Some("alt+tab"));
        assert_eq!(spoken_chord("shift enter").as_deref(), Some("shift+enter"));
        assert_eq!(
            spoken_chord("control shift t").as_deref(),
            Some("ctrl+shift+t")
        );
        assert_eq!(spoken_chord("super e").as_deref(), Some("meta+e"));
        assert_eq!(spoken_chord("alt f4").as_deref(), Some("alt+f4"));
    }

    #[test]
    fn single_keys_and_spoken_numbers() {
        assert_eq!(spoken_chord("escape").as_deref(), Some("escape"));
        assert_eq!(spoken_chord("enter").as_deref(), Some("enter"));
        assert_eq!(spoken_chord("f five").as_deref(), Some("f5"));
        assert_eq!(spoken_chord("f 11").as_deref(), Some("f11"));
        assert_eq!(spoken_chord("control one").as_deref(), Some("ctrl+1"));
        assert_eq!(spoken_chord("page down").as_deref(), Some("pagedown"));
        assert_eq!(
            spoken_chord("control page up").as_deref(),
            Some("ctrl+pageup")
        );
    }

    #[test]
    fn filler_words_are_dropped() {
        assert_eq!(spoken_chord("the escape key").as_deref(), Some("escape"));
        assert_eq!(
            spoken_chord("an escape keystroke").as_deref(),
            Some("escape")
        );
        assert_eq!(spoken_chord("control plus s").as_deref(), Some("ctrl+s"));
        assert_eq!(
            spoken_chord("control and shift and t").as_deref(),
            Some("ctrl+shift+t")
        );
        assert_eq!(spoken_chord("the down arrow").as_deref(), Some("down"));
        assert_eq!(spoken_chord("the up key").as_deref(), Some("up"));
    }

    #[test]
    fn filler_words_at_the_end_are_keys() {
        assert_eq!(spoken_chord("control a").as_deref(), Some("ctrl+a"));
        // "press" is a cue the payload rule strips before the chord is read.
        assert_eq!(spoken_chord("press a"), None);
        assert_eq!(spoken_chord("a").as_deref(), Some("a"));
        assert_eq!(spoken_chord("the a key").as_deref(), Some("a"));
        assert_eq!(spoken_chord("control plus").as_deref(), Some("ctrl+="));
        assert_eq!(spoken_chord("control minus").as_deref(), Some("ctrl+-"));
        assert_eq!(spoken_chord("shift and").as_deref(), None);
    }

    #[test]
    fn modifiers_come_first_and_never_alone() {
        assert_eq!(spoken_chord("s control").as_deref(), Some("ctrl+s"));
        assert_eq!(
            spoken_chord("t shift control").as_deref(),
            Some("shift+ctrl+t")
        );
        assert_eq!(spoken_chord("control"), None);
        assert_eq!(spoken_chord("control shift"), None);
        assert_eq!(spoken_chord("the control key"), None);
    }

    #[test]
    fn anything_that_is_not_a_key_refuses_the_whole_chord() {
        assert_eq!(spoken_chord("the any key"), None);
        assert_eq!(spoken_chord("control save"), None);
        assert_eq!(spoken_chord("hit control s"), None);
        assert_eq!(spoken_chord("f thirteen"), None);
        assert_eq!(spoken_chord("f99"), None);
        assert_eq!(spoken_chord(""), None);
        assert_eq!(spoken_chord("the key"), None);
    }
}
