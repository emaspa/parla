//! Small text helpers shared by matching code: a normal form for comparing
//! spoken phrases, and word counting the stats agree on.

/// Lowercase, punctuation stripped, whitespace collapsed to single spaces.
/// "Insert my e-mail." and "insert my email" compare equal.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(c.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

pub fn word_count(text: &str) -> u32 {
    text.split_whitespace().count() as u32
}

/// Replace every case-insensitive, whole-word occurrence of `from` in
/// `text` with `to`. Word boundaries are non-alphanumeric characters, so
/// "e-mail" matches in "my e-mail address" but "mail" does not match
/// "email".
pub fn replace_word(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_string();
    }
    let lower = text.to_lowercase();
    let needle = from.to_lowercase();
    // Char counts differ from byte counts under case mapping only for a
    // few scripts; guard by falling back to the untouched text then.
    if lower.len() != text.len() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(off) = lower[i..].find(&needle) {
        let start = i + off;
        let end = start + needle.len();
        let before_ok = start == 0
            || !text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
        let after_ok = end == text.len()
            || !text[end..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric);
        if before_ok && after_ok {
            out.push_str(&text[i..start]);
            out.push_str(to);
            i = end;
        } else {
            // Advance one char past the false start.
            let step = text[start..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&text[i..start + step]);
            i = start + step;
        }
    }
    out.push_str(&text[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_punctuation_and_case() {
        assert_eq!(normalize("Insert my e-mail."), "insert my e mail");
        assert_eq!(normalize("  Hello,   World  "), "hello world");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn replace_word_respects_boundaries() {
        assert_eq!(
            replace_word("my e-mail and E-Mail", "e-mail", "email"),
            "my email and email"
        );
        assert_eq!(replace_word("emailing", "mail", "post"), "emailing");
        assert_eq!(
            replace_word("send mail now", "mail", "post"),
            "send post now"
        );
        assert_eq!(replace_word("mail", "mail", "post"), "post");
        assert_eq!(replace_word("x", "", "y"), "x");
    }
}
