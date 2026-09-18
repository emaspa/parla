//! The message inside an utterance, found by rule rather than by asking the
//! model.
//!
//! Once the judge knows an utterance is a `notify`, `krunner`, `key` or
//! `claude_tell`, the message it carries usually follows a cue phrase:
//! "remind me to buy milk", "search for the invoice", "hit control s",
//! "tell claude to fix the test". Stripping the cue is a matter of word
//! matching, and a rule gets it right where a small model, asked to pick a
//! span, kept the whole utterance. The model is asked only when no rule
//! applies.

/// Words spoken before a cue that carry no meaning of their own.
const POLITE_PREFIXES: &[&str] = &[
    "please",
    "can you",
    "could you",
    "would you",
    "will you",
    "hey",
];

/// Cue phrases per intent, matched at the start of the utterance. Longer
/// phrases are tried first so "remind me to" wins over "remind me".
fn cues(intent: &str) -> &'static [&'static str] {
    match intent {
        "key" => &[
            "hit", "press", "push", "send", "type", "do", "the key", "keys", "key",
        ],
        "notify" => &[
            "remind me on screen that",
            "remind me on screen to",
            "remind me on screen",
            "remind me that",
            "remind me to",
            "remind me",
            "notify me that",
            "notify me to",
            "notify me",
            "notify",
            "make a note that",
            "make a note to",
            "make a note",
            "note that",
            "note",
            "set a reminder that",
            "set a reminder to",
            "set a reminder",
        ],
        "krunner" => &[
            "search for",
            "search",
            "look up",
            "look for",
            "track down",
            "locate",
            "where is",
            "where's",
            "run",
            "find",
            "krunner",
        ],
        "claude_tell" => &[
            "tell claude code to",
            "tell claude code",
            "tell claude to",
            "tell claude",
            "ask claude code to",
            "ask claude code",
            "ask claude to",
            "ask claude",
            "say to claude",
            "have claude",
            "get claude to",
            "message claude that",
            "message claude",
            "claude,",
            "claude:",
        ],
        _ => &[],
    }
}

/// Phrases that introduce quoted text in the middle of an utterance: "pop
/// up a note saying lunch is ready", "put a notification up that says call
/// mum". What follows is the message.
const INTRODUCERS: &[&str] = &[
    "saying",
    "that says",
    "which says",
    "that reads",
    "the message",
    "notification that says",
    "notification which says",
    "notification that",
    "notification saying",
];

/// A verb of saying left at the front of a message once an introducer is
/// stripped: "a notification that says lunch" after "notification that".
const SAYING: &[&str] = &["says", "saying", "reads"];

/// "the file called budget": what a thing is called is its name, and the
/// name is what KRunner wants. Only after a noun for the thing: "who
/// called me" is a search for exactly that.
const NAME_INTRODUCERS: &[&str] = &["called", "named"];
const NAMED_THINGS: &[&str] = &[
    "file",
    "document",
    "folder",
    "app",
    "application",
    "program",
    "thing",
    "one",
    "something",
];

/// A word as it is matched: lowercased, punctuation stripped, except a
/// trailing comma or colon which a cue may require ("claude,").
fn word_key(raw: &str) -> String {
    let lower = raw.to_lowercase();
    let trimmed: String = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'')
        .collect();
    match lower.chars().last() {
        Some(c @ (',' | ':')) => format!("{trimmed}{c}"),
        _ => trimmed,
    }
}

/// Does `phrase` (one or more words) start `words`? A cue word that ends in
/// punctuation must match it; other cue words match with the punctuation
/// stripped.
fn starts_with_phrase(words: &[&str], phrase: &str) -> Option<usize> {
    let cue: Vec<&str> = phrase.split_whitespace().collect();
    if words.len() < cue.len() {
        return None;
    }
    let matches = cue.iter().zip(words).all(|(c, w)| {
        let k = word_key(w);
        if c.ends_with([',', ':']) {
            k == *c
        } else {
            k.trim_end_matches([',', ':']) == *c
        }
    });
    matches.then_some(cue.len())
}

/// `phrases` longest first, so "remind me to" is tried before "remind me".
fn longest_first<'p>(phrases: &[&'p str]) -> Vec<&'p str> {
    let mut ordered: Vec<&str> = phrases.to_vec();
    ordered.sort_by_key(|p| std::cmp::Reverse(p.split_whitespace().count()));
    ordered
}

/// Strip any of `phrases` from the front of `words`, longest first, once.
fn strip_leading<'a>(words: &[&'a str], phrases: &[&str]) -> Option<Vec<&'a str>> {
    for p in longest_first(phrases) {
        if let Some(n) = starts_with_phrase(words, p) {
            return Some(words[n..].to_vec());
        }
    }
    None
}

/// Strip `phrases` from the front of `words` as often as they occur:
/// "please can you" is two prefixes.
fn strip_leading_all<'a>(words: &[&'a str], phrases: &[&str]) -> Vec<&'a str> {
    let mut words = words.to_vec();
    while let Some(rest) = strip_leading(&words, phrases) {
        words = rest;
    }
    words
}

/// Does any of `phrases` end `words`? "pass this on to claude:" ends in
/// the cue "claude:".
fn ends_with_phrase(words: &[&str], phrases: &[&str]) -> bool {
    phrases.iter().any(|p| {
        let n = p.split_whitespace().count();
        words.len() >= n && starts_with_phrase(&words[words.len() - n..], p).is_some()
    })
}

/// Where `phrases` first occur inside `words`, the words after it. At one
/// position the longest phrase wins, so "notification that says" is not
/// cut at "notification that", and a verb of saying left in front of the
/// message is dropped.
fn after_introducer<'a>(words: &[&'a str], phrases: &[&str]) -> Option<Vec<&'a str>> {
    let ordered = longest_first(phrases);
    for start in 0..words.len() {
        for p in &ordered {
            if let Some(n) = starts_with_phrase(&words[start..], p) {
                let rest = strip_leading_all(&words[start + n..], SAYING);
                if !rest.is_empty() {
                    return Some(rest);
                }
            }
        }
    }
    None
}

/// "the file called budget": the words after "called" or "named" when a
/// noun for the thing comes right before it.
fn after_name_introducer<'a>(words: &[&'a str]) -> Option<Vec<&'a str>> {
    for start in 1..words.len() {
        if !NAMED_THINGS.contains(&word_key(words[start - 1]).as_str()) {
            continue;
        }
        if let Some(rest) = strip_leading(&words[start..], NAME_INTRODUCERS) {
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

/// Drop every "please" at the end: "fix it please please" is "fix it",
/// and "please" alone is no message.
fn drop_trailing_please(words: &mut Vec<&str>) {
    while words.last().is_some_and(|w| word_key(w) == "please") {
        words.pop();
    }
}

fn join(words: &[&str]) -> String {
    let mut text = words.join(" ");
    // A comma left behind by a dropped "please": "fix it, please".
    while text.ends_with([',', ';']) {
        text.pop();
    }
    text.trim().to_string()
}

/// The message `utterance` carries for `intent`, when a rule finds one.
/// None means no cue applied and the model has to be asked. The text is
/// returned as spoken, with only the wrapper removed.
pub fn strip_cue(intent: &str, utterance: &str) -> Option<String> {
    let cues = cues(intent);
    if cues.is_empty() {
        return None;
    }
    // "flash a message: build finished". A colon followed by a space
    // introduces the quoted part for any intent that passes text on, when
    // the words before it are the wrapper: they end in a cue or an
    // introducer ("pass this on to claude:", "send claude the message:"),
    // or no cue at the start claims the utterance. In "tell claude to
    // email Bob: the report is late" the colon is Bob's.
    let colon = (intent != "key")
        .then(|| utterance.split_once(": "))
        .flatten()
        .and_then(|(head, after)| {
            let mut words: Vec<&str> = after.split_whitespace().collect();
            drop_trailing_please(&mut words);
            let head: Vec<&str> = head.split_whitespace().collect();
            let wrapper = ends_with_phrase(&head, cues) || ends_with_phrase(&head, INTRODUCERS);
            (!words.is_empty()).then_some((wrapper, words))
        });
    if let Some((true, words)) = &colon {
        return Some(join(words));
    }
    let mut words: Vec<&str> = utterance.split_whitespace().collect();
    drop_trailing_please(&mut words);
    words = strip_leading_all(&words, POLITE_PREFIXES);
    let mut rest = strip_leading(&words, cues);
    if rest.is_none() && matches!(intent, "notify" | "claude_tell") {
        rest = after_introducer(&words, INTRODUCERS);
    }
    let mut rest = match (rest, colon) {
        (Some(rest), _) => rest,
        (None, Some((_, after))) => after,
        (None, None) => return None,
    };
    if intent == "krunner" {
        if let Some(name) = after_name_introducer(&rest) {
            rest = name;
        }
    }
    if rest.is_empty() {
        return None;
    }
    Some(join(&rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(intent: &str, s: &str) -> Option<String> {
        strip_cue(intent, s)
    }

    #[test]
    fn key_cues() {
        assert_eq!(strip("key", "hit control s").as_deref(), Some("control s"));
        assert_eq!(strip("key", "press enter").as_deref(), Some("enter"));
        assert_eq!(
            strip("key", "send control shift t").as_deref(),
            Some("control shift t")
        );
        assert_eq!(strip("key", "push alt f4").as_deref(), Some("alt f4"));
        assert_eq!(
            strip("key", "do control z please").as_deref(),
            Some("control z")
        );
        assert_eq!(strip("key", "the key escape").as_deref(), Some("escape"));
        assert_eq!(strip("key", "could you press tab").as_deref(), Some("tab"));
        assert_eq!(
            strip("key", "please can you press enter").as_deref(),
            Some("enter")
        );
        assert_eq!(strip("key", "press a").as_deref(), Some("a"));
        // No cue: the model is asked.
        assert_eq!(strip("key", "control s"), None);
        assert_eq!(strip("key", "escape"), None);
        // A cue with nothing after it is not a payload.
        assert_eq!(strip("key", "press"), None);
    }

    #[test]
    fn notify_cues() {
        assert_eq!(
            strip("notify", "remind me to buy milk").as_deref(),
            Some("buy milk")
        );
        assert_eq!(
            strip("notify", "remind me on screen that the meeting is at three").as_deref(),
            Some("the meeting is at three")
        );
        assert_eq!(
            strip("notify", "notify me when the build is done").as_deref(),
            Some("when the build is done")
        );
        assert_eq!(
            strip("notify", "make a note that rent is due").as_deref(),
            Some("rent is due")
        );
        assert_eq!(
            strip("notify", "set a reminder to call mum").as_deref(),
            Some("call mum")
        );
        assert_eq!(
            strip("notify", "Note: buy eggs").as_deref(),
            Some("buy eggs")
        );
        assert_eq!(
            strip("notify", "pop up a note saying lunch is ready").as_deref(),
            Some("lunch is ready")
        );
        assert_eq!(
            strip("notify", "put a notification up that says call mum").as_deref(),
            Some("call mum")
        );
        assert_eq!(
            strip("notify", "flash a message: build finished").as_deref(),
            Some("build finished")
        );
        assert_eq!(
            strip(
                "notify",
                "let me know with a notification that the pizza is here"
            )
            .as_deref(),
            Some("the pizza is here")
        );
        assert_eq!(
            strip("notify", "show a notification that says the build finished").as_deref(),
            Some("the build finished")
        );
        assert_eq!(
            strip("notify", "show a notification which says lunch").as_deref(),
            Some("lunch")
        );
        assert_eq!(
            strip("notify", "with a notification saying that's it").as_deref(),
            Some("that's it")
        );
        assert_eq!(strip("notify", "show a note on screen"), None);
    }

    #[test]
    fn krunner_cues() {
        assert_eq!(
            strip("krunner", "search for the invoice").as_deref(),
            Some("the invoice")
        );
        assert_eq!(
            strip("krunner", "look up last year's tax return").as_deref(),
            Some("last year's tax return")
        );
        assert_eq!(
            strip("krunner", "can you look for the vacation photos").as_deref(),
            Some("the vacation photos")
        );
        assert_eq!(
            strip("krunner", "track down my thesis draft").as_deref(),
            Some("my thesis draft")
        );
        assert_eq!(
            strip("krunner", "where is the file called budget").as_deref(),
            Some("budget")
        );
        assert_eq!(
            strip("krunner", "find the document named report").as_deref(),
            Some("report")
        );
        assert_eq!(strip("krunner", "run htop").as_deref(), Some("htop"));
        // "called" names a thing only after a noun for one.
        assert_eq!(
            strip("krunner", "look up who called me").as_deref(),
            Some("who called me")
        );
        assert_eq!(
            strip("krunner", "find the one called notes").as_deref(),
            Some("notes")
        );
        assert_eq!(strip("krunner", "I want the vacation photos"), None);
    }

    #[test]
    fn claude_tell_cues() {
        assert_eq!(
            strip("claude_tell", "tell claude to fix the failing test").as_deref(),
            Some("fix the failing test")
        );
        assert_eq!(
            strip("claude_tell", "tell claude code the tests pass").as_deref(),
            Some("the tests pass")
        );
        assert_eq!(
            strip("claude_tell", "ask claude why this fails").as_deref(),
            Some("why this fails")
        );
        assert_eq!(
            strip("claude_tell", "Claude, add a readme please").as_deref(),
            Some("add a readme")
        );
        assert_eq!(
            strip("claude_tell", "have claude explain the last error").as_deref(),
            Some("explain the last error")
        );
        assert_eq!(
            strip("claude_tell", "get claude to add a readme").as_deref(),
            Some("add a readme")
        );
        assert_eq!(
            strip("claude_tell", "message claude that the tests are green now").as_deref(),
            Some("the tests are green now")
        );
        assert_eq!(
            strip("claude_tell", "pass this on to claude: refactor the parser").as_deref(),
            Some("refactor the parser")
        );
        assert_eq!(
            strip("claude_tell", "send claude the message: run the linter").as_deref(),
            Some("run the linter")
        );
        // "claude" without the comma is not the cue: "claude fix it" is
        // left to the model.
        assert_eq!(strip("claude_tell", "claude fix it"), None);
        assert_eq!(strip("claude_tell", "tell claude"), None);
        assert_eq!(strip("claude_tell", "tell claude: please"), None);
        assert_eq!(strip("claude_tell", "tell claude please please"), None);
        // A colon inside the message is not the wrapper's.
        assert_eq!(
            strip("claude_tell", "tell claude to email Bob: the report is late").as_deref(),
            Some("email Bob: the report is late")
        );
        assert_eq!(
            strip("claude_tell", "tell claude: fix the build").as_deref(),
            Some("fix the build")
        );
    }

    #[test]
    fn text_is_kept_as_spoken() {
        assert_eq!(
            strip(
                "claude_tell",
                "tell Claude to Rename `Foo` to `Bar`, please"
            )
            .as_deref(),
            Some("Rename `Foo` to `Bar`")
        );
        assert_eq!(strip("show_app", "tell claude to fix it"), None);
    }
}
