/// Utterance normalization: whisper output is free-form text with punctuation
/// and capitalization we don't want the grammar to care about.

/// Lowercase, strip punctuation (keep inner apostrophes), collapse whitespace,
/// and convert spelled-out small numbers to digits ("desktop two" -> "desktop 2").
pub fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let chars: Vec<char> = lowered.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_alphanumeric() || c == ' ' {
            out.push(c);
        } else if c == '\'' {
            // keep apostrophes only between letters ("don't", "kate's")
            let prev_alpha = i > 0 && chars[i - 1].is_alphabetic();
            let next_alpha = i + 1 < chars.len() && chars[i + 1].is_alphabetic();
            if prev_alpha && next_alpha {
                out.push(c);
            } else {
                out.push(' ');
            }
        } else {
            out.push(' ');
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    numbers_to_digits(&collapsed)
}

const NUMBER_WORDS: [(&str, u32); 20] = [
    ("one", 1), ("two", 2), ("three", 3), ("four", 4), ("five", 5),
    ("six", 6), ("seven", 7), ("eight", 8), ("nine", 9), ("ten", 10),
    ("eleven", 11), ("twelve", 12), ("thirteen", 13), ("fourteen", 14),
    ("fifteen", 15), ("sixteen", 16), ("seventeen", 17), ("eighteen", 18),
    ("nineteen", 19), ("twenty", 20),
];

fn numbers_to_digits(text: &str) -> String {
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
    fn converts_number_words() {
        assert_eq!(normalize("go to desktop two"), "go to desktop 2");
        assert_eq!(normalize("desktop twelve"), "desktop 12");
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
