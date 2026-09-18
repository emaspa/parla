//! Spellings learned from corrections. When the user fixes a word in the
//! field right after a dictation, the daemon records the pair here; the
//! UI offers it, and with `flow.learn = "auto"` the daemon moves a pair
//! seen twice into the dictionary itself.
//!
//! ```toml
//! dismissed = ["monday->Mondays"]
//!
//! [[suggestion]]
//! heard = "Emanuel"
//! written = "Emanuele"
//! count = 2
//! last_at_ms = 1758190000123
//! app = "kmail"
//! ```
//!
//! `dismissed` holds the keys of pairs the user turned down, so they are
//! not offered again; it comes first because a plain key after a
//! `[[suggestion]]` table would belong to that table. [`corrections`] is the comparison itself: what parla
//! typed against the same region read back later, at word level.

use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::dictionary::{Dictionary, Replacement};
use crate::history::Record;

/// Largest edit distance, as a share of the longer word, at which two
/// words still count as spellings of the same word.
const MAX_DISTANCE: f64 = 0.34;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Suggestions {
    #[serde(rename = "suggestion")]
    pub suggestions: Vec<Suggestion>,
    /// Keys, as [`key`] makes them, of pairs the user does not want.
    pub dismissed: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Suggestion {
    /// What parla typed.
    pub heard: String,
    /// What the user changed it to.
    pub written: String,
    /// How many dictations ended with this correction.
    pub count: u32,
    /// Unix time in milliseconds of the last one.
    pub last_at_ms: u64,
    /// The application it was seen in last, as its toolkit names it.
    pub app: String,
}

impl Suggestion {
    pub fn key(&self) -> String {
        key(&self.heard, &self.written)
    }
}

/// The key a pair is dismissed and accepted by: the heard word lowercased,
/// so "Emanuel" and "emanuel" corrected to the same spelling are one pair,
/// and the written word as is, since its capitalisation is the point.
pub fn key(heard: &str, written: &str) -> String {
    format!("{}->{}", heard.trim().to_lowercase(), written.trim())
}

impl Suggestions {
    /// Read the file, or an empty list when it does not exist yet.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(toml_str: &str) -> anyhow::Result<Self> {
        let mut s: Self = toml::from_str(toml_str)?;
        s.tidy();
        Ok(s)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut s = self.clone();
        s.tidy();
        crate::paths::write_atomic(path, &toml::to_string_pretty(&s)?)
    }

    /// Trim, drop empty and identity pairs, merge duplicates by key (counts
    /// add up, the latest time and app win), drop what was dismissed.
    fn tidy(&mut self) {
        let mut seen = std::collections::BTreeSet::new();
        self.dismissed = self
            .dismissed
            .iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty() && seen.insert(k.clone()))
            .collect();
        let mut merged: Vec<Suggestion> = Vec::new();
        for s in self.suggestions.drain(..) {
            let heard = s.heard.trim().to_string();
            let written = s.written.trim().to_string();
            if heard.is_empty() || written.is_empty() || heard == written {
                continue;
            }
            let k = key(&heard, &written);
            if self.dismissed.contains(&k) {
                continue;
            }
            match merged.iter_mut().find(|m| m.key() == k) {
                Some(m) => {
                    m.count = m.count.saturating_add(s.count.max(1));
                    if s.last_at_ms >= m.last_at_ms {
                        m.last_at_ms = s.last_at_ms;
                        m.app = s.app.trim().to_string();
                    }
                }
                None => merged.push(Suggestion {
                    heard,
                    written,
                    count: s.count.max(1),
                    last_at_ms: s.last_at_ms,
                    app: s.app.trim().to_string(),
                }),
            }
        }
        self.suggestions = merged;
    }

    pub fn is_dismissed(&self, heard: &str, written: &str) -> bool {
        self.dismissed.contains(&key(heard, written))
    }

    /// Note one more sighting of `heard` corrected to `written` in `app`,
    /// now. Returns the count so far, or 0 for a dismissed pair, which is
    /// not recorded.
    pub fn record(&mut self, heard: &str, written: &str, app: &str) -> u32 {
        self.record_at(heard, written, app, Record::now_ms())
    }

    pub fn record_at(&mut self, heard: &str, written: &str, app: &str, now_ms: u64) -> u32 {
        let heard = heard.trim();
        let written = written.trim();
        if heard.is_empty() || written.is_empty() || heard == written {
            return 0;
        }
        if self.is_dismissed(heard, written) {
            return 0;
        }
        let k = key(heard, written);
        if let Some(s) = self.suggestions.iter_mut().find(|s| s.key() == k) {
            s.count = s.count.saturating_add(1);
            s.last_at_ms = now_ms;
            s.app = app.trim().to_string();
            return s.count;
        }
        self.suggestions.push(Suggestion {
            heard: heard.to_string(),
            written: written.to_string(),
            count: 1,
            last_at_ms: now_ms,
            app: app.trim().to_string(),
        });
        1
    }

    /// Drop the pair and remember not to offer it again.
    pub fn dismiss(&mut self, key: &str) {
        let key = key.trim();
        if key.is_empty() {
            return;
        }
        self.suggestions.retain(|s| s.key() != key);
        if !self.dismissed.iter().any(|k| k == key) {
            self.dismissed.push(key.to_string());
        }
    }

    /// Take the pair out, as the replacement the dictionary should get.
    /// None when there is no such pair.
    pub fn accept(&mut self, key: &str) -> Option<Replacement> {
        let i = self
            .suggestions
            .iter()
            .position(|s| s.key() == key.trim())?;
        let s = self.suggestions.remove(i);
        Some(Replacement {
            spoken: s.heard,
            written: s.written,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.suggestions.is_empty()
    }
}

/// Whether the dictionary already produces `written`: it is one of the
/// words, or a replacement already maps `heard` somewhere.
pub fn covered(dictionary: &Dictionary, heard: &str, written: &str) -> bool {
    dictionary.words.iter().any(|w| w == written.trim())
        || dictionary
            .replacements
            .iter()
            .any(|r| r.spoken.eq_ignore_ascii_case(heard.trim()))
}

/// Add an accepted pair to the dictionary: `written` to the words, unless
/// a word already spells it (case-insensitively), and a replacement from
/// `spoken`, unless one for that word exists.
pub fn add_to_dictionary(dictionary: &mut Dictionary, r: Replacement) {
    if !dictionary
        .words
        .iter()
        .any(|w| w.eq_ignore_ascii_case(&r.written))
    {
        dictionary.words.push(r.written.clone());
    }
    if !dictionary
        .replacements
        .iter()
        .any(|x| x.spoken.eq_ignore_ascii_case(&r.spoken))
    {
        dictionary.replacements.push(r);
    }
}

/// The words in `now` that stand where a different word stood in `typed`
/// and look like another spelling of it. `typed` is what parla typed (with
/// whatever context was around it) and `now` the same region read back
/// later. The two are aligned word by word on their longest common
/// subsequence; a run of words replaced by a run of the same length is
/// taken pairwise, and a pair counts when both words are letters (with
/// apostrophes and hyphens), differ, and are equal ignoring case and
/// diacritics or within [`MAX_DISTANCE`] edits of each other. Inserted and
/// deleted words are ignored.
pub fn corrections(typed: &str, now: &str) -> Vec<(String, String)> {
    let a = words(typed);
    let b = words(now);
    let mut out = Vec::new();
    let mut ai = 0;
    let mut bi = 0;
    for (i, j) in lcs(&a, &b).into_iter().chain([(a.len(), b.len())]) {
        if i - ai == j - bi {
            for k in 0..i - ai {
                let (x, y) = (a[ai + k], b[bi + k]);
                if spelling_pair(x, y) {
                    out.push((x.to_string(), y.to_string()));
                }
            }
        }
        ai = i + 1;
        bi = j + 1;
    }
    out
}

/// Whitespace-separated tokens with surrounding punctuation stripped.
fn words(s: &str) -> Vec<&str> {
    s.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect()
}

/// Index pairs of one longest common subsequence, in order.
fn lcs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let mut len = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            len[i][j] = if a[i] == b[j] {
                len[i + 1][j + 1] + 1
            } else {
                len[i + 1][j].max(len[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if len[i + 1][j] >= len[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn wordlike(w: &str) -> bool {
    w.chars().any(char::is_alphabetic)
        && w.chars()
            .all(|c| c.is_alphabetic() || matches!(c, '\'' | '’' | '-'))
}

/// Two different words that are spellings of the same word.
fn spelling_pair(a: &str, b: &str) -> bool {
    if a == b || !wordlike(a) || !wordlike(b) {
        return false;
    }
    let fa = fold(a);
    let fb = fold(b);
    if fa == fb {
        return true;
    }
    let la: Vec<char> = a.to_lowercase().chars().collect();
    let lb: Vec<char> = b.to_lowercase().chars().collect();
    let longer = la.len().max(lb.len());
    levenshtein(&la, &lb) as f64 <= MAX_DISTANCE * longer as f64
}

/// Lowercase with common Latin diacritics removed, for "equal ignoring
/// case and diacritics".
fn fold(w: &str) -> String {
    w.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
            'ç' | 'ć' | 'č' => 'c',
            'ď' | 'đ' => 'd',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e',
            'ğ' => 'g',
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' => 'i',
            'ł' => 'l',
            'ñ' | 'ń' | 'ň' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
            'ř' => 'r',
            'ś' | 'š' | 'ş' => 's',
            'ť' => 't',
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => 'u',
            'ý' | 'ÿ' => 'y',
            'ź' | 'ż' | 'ž' => 'z',
            other => other,
        })
        .collect()
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, &ca) in a.iter().enumerate() {
        let mut prev_diag = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let next = (row[j + 1] + 1).min(row[j] + 1).min(prev_diag + cost);
            prev_diag = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(typed: &str, now: &str) -> Vec<(String, String)> {
        corrections(typed, now)
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn a_respelled_name_is_a_correction() {
        assert_eq!(
            pairs("I met Emanuel today.", "I met Emanuele today."),
            vec![pair("Emanuel", "Emanuele")]
        );
        // punctuation around the word is not part of it
        assert_eq!(
            pairs("Hi Emanuel,", "Hi Emanuele,"),
            vec![pair("Emanuel", "Emanuele")]
        );
    }

    #[test]
    fn a_different_word_is_not() {
        assert_eq!(pairs("see you Monday", "see you Tuesday"), vec![]);
        assert_eq!(pairs("the cat", "a cat"), vec![]);
    }

    #[test]
    fn case_and_diacritics_count_as_spelling() {
        assert_eq!(
            pairs("try parla", "try Parla"),
            vec![pair("parla", "Parla")]
        );
        assert_eq!(
            pairs("in Zurich", "in Zürich"),
            vec![pair("Zurich", "Zürich")]
        );
    }

    #[test]
    fn a_run_of_words_pairs_up() {
        assert_eq!(
            pairs("like wisper flow does", "like Wispr Flow does"),
            vec![pair("wisper", "Wispr"), pair("flow", "Flow")]
        );
    }

    #[test]
    fn unchanged_inserted_and_deleted_text_give_nothing() {
        assert_eq!(pairs("send it tuesday", "send it tuesday"), vec![]);
        assert_eq!(pairs("send it tuesday", "send it on tuesday"), vec![]);
        assert_eq!(pairs("send it on tuesday", "send it tuesday"), vec![]);
        assert_eq!(pairs("", "anything"), vec![]);
        assert_eq!(pairs("anything", ""), vec![]);
        // more text after the region is an insertion, not a correction
        assert_eq!(
            pairs("Dear Emanuel, thanks", "Dear Emanuele, thanks for the"),
            vec![pair("Emanuel", "Emanuele")]
        );
    }

    #[test]
    fn numbers_and_symbols_are_not_spellings() {
        assert_eq!(pairs("room 101", "room 102"), vec![]);
        assert_eq!(pairs("run foo/bar", "run foo/baz"), vec![]);
        assert_eq!(pairs("don't", "dont"), vec![pair("don't", "dont")]);
    }

    #[test]
    fn keys_normalise_the_heard_word_only() {
        assert_eq!(key("Emanuel", "Emanuele"), "emanuel->Emanuele");
        assert_eq!(key(" EMANUEL ", " Emanuele "), "emanuel->Emanuele");
    }

    #[test]
    fn record_bumps_and_dismiss_keeps_away() {
        let mut s = Suggestions::default();
        assert_eq!(s.record_at("Emanuel", "Emanuele", "kmail", 10), 1);
        assert_eq!(s.record_at("emanuel", "Emanuele", "kate", 20), 2);
        assert_eq!(s.suggestions.len(), 1);
        assert_eq!(s.suggestions[0].heard, "Emanuel");
        assert_eq!(s.suggestions[0].app, "kate");
        assert_eq!(s.suggestions[0].last_at_ms, 20);
        assert_eq!(s.record_at("same", "same", "x", 1), 0);
        s.dismiss("emanuel->Emanuele");
        assert!(s.is_empty());
        assert_eq!(s.record_at("Emanuel", "Emanuele", "kmail", 30), 0);
        assert!(s.is_dismissed("EMANUEL", "Emanuele"));
        assert_eq!(s.dismissed, vec!["emanuel->Emanuele"]);
        s.dismiss("emanuel->Emanuele");
        assert_eq!(s.dismissed.len(), 1);
    }

    #[test]
    fn accept_hands_over_a_replacement() {
        let mut s = Suggestions::default();
        s.record_at("wisper", "Wispr", "firefox", 1);
        assert_eq!(s.accept("nothing->here"), None);
        let r = s.accept("wisper->Wispr").unwrap();
        assert_eq!(r.spoken, "wisper");
        assert_eq!(r.written, "Wispr");
        assert!(s.is_empty());
        let mut d = Dictionary::default();
        add_to_dictionary(&mut d, r.clone());
        add_to_dictionary(&mut d, r);
        assert_eq!(d.words, vec!["Wispr"]);
        assert_eq!(d.replacements.len(), 1);
    }

    #[test]
    fn covered_pairs_are_the_dictionary_s_business() {
        let d = Dictionary::parse(
            r#"
            words = ["Emanuele"]
            [[replace]]
            spoken = "kay win"
            written = "KWin"
            "#,
        )
        .unwrap();
        assert!(covered(&d, "Emanuel", "Emanuele"));
        assert!(covered(&d, "Kay Win", "Kwin"));
        assert!(!covered(&d, "parla", "Parla"));
    }

    #[test]
    fn parses_tidies_and_round_trips() {
        let s = Suggestions::parse(
            r#"
            dismissed = ["old->Old", "old->Old", ""]
            [[suggestion]]
            heard = "Emanuel"
            written = "Emanuele"
            count = 1
            last_at_ms = 5
            app = "kmail"
            [[suggestion]]
            heard = "emanuel"
            written = "Emanuele"
            count = 2
            last_at_ms = 9
            app = "kate"
            [[suggestion]]
            heard = "old"
            written = "Old"
            [[suggestion]]
            heard = ""
            written = "x"
            "#,
        )
        .unwrap();
        assert_eq!(s.dismissed, vec!["old->Old"]);
        assert_eq!(s.suggestions.len(), 1);
        assert_eq!(s.suggestions[0].count, 3);
        assert_eq!(s.suggestions[0].last_at_ms, 9);
        assert_eq!(s.suggestions[0].app, "kate");
        let text = toml::to_string_pretty(&s).unwrap();
        assert_eq!(Suggestions::parse(&text).unwrap(), s);
        assert!(Suggestions::parse("sugestion = []").is_err());
        assert!(Suggestions::parse("").unwrap().is_empty());
    }

    #[test]
    fn load_and_save_through_a_file() {
        let dir = std::env::temp_dir().join(format!("parla-learned-{}", std::process::id()));
        let path = dir.join("learned.toml");
        let mut s = Suggestions::load(&path).unwrap();
        assert!(s.is_empty());
        s.record_at("Emanuel", "Emanuele", "kmail", 1);
        s.save(&path).unwrap();
        let back = Suggestions::load(&path).unwrap();
        assert_eq!(back, s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
