//! Normalize words for matching while retaining their original UTF-8 spans.

use std::ops::Range;

/// Lowercase, strip punctuation (keep inner apostrophes), and collapse
/// whitespace. Number words stay intact; only numeric grammar slots convert them.
pub fn normalize(text: &str) -> String {
    words_with_spans(text)
        .into_iter()
        .map(|word| word.normalized)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A matching word and its byte range in the original utterance. The first
/// and last words of each whitespace-delimited fragment include its surrounding
/// punctuation so captures retain quotes, flags and path prefixes.
pub(crate) struct MappedWord {
    pub normalized: String,
    pub raw: Range<usize>,
}

pub(crate) fn words_with_spans(text: &str) -> Vec<MappedWord> {
    let mut words = Vec::new();
    let mut cursor = 0;
    for fragment in text.split_whitespace() {
        let base = cursor + text[cursor..].find(fragment).unwrap();
        cursor = base + fragment.len();
        let first = words.len();
        let mut start = None;
        let mut normalized = String::new();
        let mut chars = fragment.char_indices().peekable();
        let mut prev_alpha = false;
        while let Some((offset, c)) = chars.next() {
            let inner_apostrophe = c == '\''
                && prev_alpha
                && chars.peek().is_some_and(|(_, next)| next.is_alphabetic());
            if c.is_alphanumeric() || inner_apostrophe {
                start.get_or_insert(base + offset);
                normalized.extend(c.to_lowercase());
            } else if let Some(start) = start.take() {
                words.push(MappedWord {
                    normalized: std::mem::take(&mut normalized),
                    raw: start..base + offset,
                });
            }
            prev_alpha = c.is_alphabetic();
        }
        if let Some(start) = start {
            words.push(MappedWord {
                normalized,
                raw: start..cursor,
            });
        }
        if words.len() > first {
            words[first].raw.start = base;
            words.last_mut().unwrap().raw.end = cursor;
        }
    }
    words
}

const NUMBER_WORDS: [(&str, u32); 20] = [
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
    ("ten", 10),
    ("eleven", 11),
    ("twelve", 12),
    ("thirteen", 13),
    ("fourteen", 14),
    ("fifteen", 15),
    ("sixteen", 16),
    ("seventeen", 17),
    ("eighteen", 18),
    ("nineteen", 19),
    ("twenty", 20),
];

pub(crate) fn numbers_to_digits(text: &str) -> String {
    text.split(' ')
        .map(|w| {
            NUMBER_WORDS
                .iter()
                .find(|(word, _)| *word == w)
                .map(|(_, n)| n.to_string())
                .unwrap_or_else(|| w.to_string())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strip a leading "claude " from model names: whisper often hears
/// "claude model claude haiku" or the user says "switch to claude opus".
pub fn clean_model_name(model: &str) -> String {
    let m = model.trim().to_lowercase();
    let m = m.strip_prefix("claude ").unwrap_or(&m);
    // tolerate "claude 3 haiku" style
    m.replace("claude ", "").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_punctuation_and_case() {
        assert_eq!(normalize("Open Firefox, please!"), "open firefox please");
    }

    #[test]
    fn keeps_inner_apostrophes() {
        assert_eq!(
            normalize("don't close Kate's window"),
            "don't close kate's window"
        );
    }

    #[test]
    fn preserves_number_words() {
        assert_eq!(normalize("go to desktop two"), "go to desktop two");
        assert_eq!(normalize("desktop twelve"), "desktop twelve");
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(normalize("  focus   firefox  "), "focus firefox");
    }

    #[test]
    fn model_name_cleanup() {
        assert_eq!(clean_model_name("Claude Haiku"), "haiku");
        assert_eq!(clean_model_name("opus"), "opus");
    }
}
