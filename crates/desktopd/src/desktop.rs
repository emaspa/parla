//! Installed applications: the `.desktop` index and fuzzy lookup for spoken
//! queries. Runtime state about this machine, not grammar, so it lives next
//! to the launcher that consumes it.

use std::path::{Path, PathBuf};

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

fn xdg_data_dirs(
    home: Option<&Path>,
    data_home: Option<PathBuf>,
    data_dirs: Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(user) = data_home
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".local/share")))
    {
        dirs.push(user);
    }
    let system: Vec<_> = data_dirs
        .as_deref()
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|p| p.is_absolute())
        .collect();
    if system.is_empty() {
        dirs.extend([
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]);
    } else {
        dirs.extend(system);
    }
    if let Some(home) = home {
        dirs.push(home.join(".local/share/flatpak/exports/share"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    dirs
}

fn can_execute(binary: &str) -> bool {
    fn executable(path: &Path) -> bool {
        let Ok(metadata) = path.metadata() else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
    let path = Path::new(binary);
    if path.is_absolute() {
        return executable(path);
    }
    if binary.is_empty() || path.components().count() != 1 {
        return false;
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| executable(&dir.join(binary))))
}

/// Lowercase words with punctuation stripped, keeping inner apostrophes
/// ("kate's"). Whisper transcripts arrive with case and commas; entry names
/// are matched without them.
fn normalize(text: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for fragment in text.split_whitespace() {
        let mut word = String::new();
        let mut chars = fragment.char_indices().peekable();
        let mut prev_alpha = false;
        while let Some((_, c)) = chars.next() {
            let inner_apostrophe = c == '\''
                && prev_alpha
                && chars.peek().is_some_and(|(_, next)| next.is_alphabetic());
            if c.is_alphanumeric() || inner_apostrophe {
                word.extend(c.to_lowercase());
            } else if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            prev_alpha = c.is_alphabetic();
        }
        if !word.is_empty() {
            words.push(word);
        }
    }
    words.join(" ")
}

fn content_words(text: &str) -> Vec<String> {
    let normalized = normalize(text);
    let mut words = normalized.split_whitespace().peekable();
    let mut content = Vec::new();
    while let Some(word) = words.next() {
        if matches!(word, "please" | "the" | "a" | "an" | "up") {
            continue;
        }
        if word == "for" && words.peek() == Some(&"me") {
            words.next();
            continue;
        }
        content.push(word.to_string());
    }
    content
}

/// A parsed .desktop application entry.
#[derive(Debug, Clone)]
pub struct DesktopEntry {
    /// Desktop id, e.g. `org.kde.dolphin.desktop` (path relative to the
    /// applications dir with separators replaced by hyphens).
    pub id: String,
    pub name: String,
    pub generic_name: Option<String>,
    pub keywords: Vec<String>,
    pub exec: Option<String>,
    pub terminal: bool,
}

impl DesktopEntry {
    /// Haystack used for fuzzy matching, most identifying first.
    fn match_targets(&self) -> Vec<String> {
        let mut t = vec![self.name.clone()];
        if let Some(g) = &self.generic_name {
            t.push(g.clone());
        }
        t.extend(self.keywords.iter().cloned());
        // id without vendor prefix/extension: "org.kde.dolphin.desktop" -> "dolphin"
        let stem = self.id.trim_end_matches(".desktop");
        let last = stem.rsplit('.').next().unwrap_or(stem);
        t.push(last.replace(['-', '_'], " "));
        t
    }
}

/// Index of installed .desktop entries with fuzzy lookup for voice queries.
pub struct DesktopIndex {
    entries: Vec<DesktopEntry>,
    matcher: SkimMatcherV2,
    min_score: i64,
}

impl DesktopIndex {
    /// Load from the standard XDG application directories.
    pub fn from_xdg() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
        let data_dirs = std::env::var_os("XDG_DATA_DIRS");
        Self::from_dirs(&xdg_data_dirs(home.as_deref(), data_home, data_dirs))
    }

    /// Load data directories in precedence order, appending `applications`
    /// exactly once to each directory. The first occurrence of each id wins.
    pub fn from_dirs(data_dirs: &[PathBuf]) -> Self {
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for dir in data_dirs {
            let applications = dir.join("applications");
            Self::load_dir(&applications, &applications, &mut entries, &mut seen);
        }
        tracing::info!("desktop index: {} entries", entries.len());
        Self {
            entries,
            matcher: SkimMatcherV2::default(),
            min_score: 40,
        }
    }

    fn load_dir(
        dir: &Path,
        applications: &Path,
        out: &mut Vec<DesktopEntry>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                Self::load_dir(&path, applications, out, seen);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(relative) = path.strip_prefix(applications) else {
                continue;
            };
            let Some(parts) = relative
                .iter()
                .map(|part| part.to_str())
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let id = parts.join("-");
            // first dir in XDG order wins for duplicate ids
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(entry) = Self::parse(&path, &id) {
                out.push(entry);
            }
        }
    }

    fn parse(path: &Path, id: &str) -> Option<DesktopEntry> {
        let content = std::fs::read_to_string(path).ok()?;
        let mut in_entry = false;
        let mut name = None;
        let mut generic_name = None;
        let mut keywords = Vec::new();
        let mut exec = None;
        let mut terminal = false;
        let mut hidden = false;
        let mut entry_type = None;
        let mut try_exec = None;

        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_entry = line == "[Desktop Entry]";
                continue;
            }
            if !in_entry || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            // skip localized keys: Name[de]=
            if key.contains('[') {
                continue;
            }
            match key.trim() {
                "Type" => entry_type = Some(value.trim()),
                "TryExec" => try_exec = Some(value.trim()),
                "Name" => name = Some(value.trim().to_string()),
                "GenericName" => generic_name = Some(value.trim().to_string()),
                "Keywords" => keywords.extend(
                    value
                        .split(';')
                        .map(|k| k.trim().to_lowercase())
                        .filter(|k| !k.is_empty()),
                ),
                "Exec" => exec = Some(value.trim().to_string()),
                "Terminal" => terminal = value.trim() == "true",
                "NoDisplay" | "Hidden" => hidden |= value.trim() == "true",
                _ => {}
            }
        }
        if hidden
            || entry_type != Some("Application")
            || try_exec.is_some_and(|binary| !can_execute(binary))
        {
            return None;
        }
        Some(DesktopEntry {
            id: id.to_string(),
            name: name?,
            generic_name,
            keywords,
            exec,
            terminal,
        })
    }

    pub fn entries(&self) -> &[DesktopEntry] {
        &self.entries
    }

    /// The entry with exactly this desktop id, for callers that chose one
    /// from [`Self::entries`] or [`Self::shortlist`] earlier and want it
    /// back without a second fuzzy match.
    pub fn by_id(&self, id: &str) -> Option<&DesktopEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Plausible entries for a whole utterance, best first.
    ///
    /// [`Self::lookup`] needs an already-isolated app name; this takes the raw
    /// words and scores every entry against each of them, so "bring up fire
    /// fox for me" still surfaces Firefox. Callers that cannot isolate the
    /// name themselves use this to narrow the field before choosing.
    pub fn shortlist(&self, text: &str, limit: usize) -> Vec<&DesktopEntry> {
        let words: Vec<String> = content_words(text)
            .into_iter()
            .filter(|w| w.len() >= 3)
            .collect();
        if words.is_empty() {
            return Vec::new();
        }

        // Probe with each content word, plus the whole utterance so multi-word
        // names ("visual studio code") can still win.
        let mut probes: Vec<Vec<String>> = words.iter().map(|w| vec![w.clone()]).collect();
        probes.push(words);

        let mut scored: Vec<(i64, &DesktopEntry)> = Vec::new();
        for e in &self.entries {
            let targets: Vec<String> = e.match_targets().iter().map(|t| t.to_lowercase()).collect();
            let best = probes
                .iter()
                .filter_map(|probe| self.score(&targets, probe))
                .max();
            if let Some(score) = best {
                if score >= self.min_score {
                    scored.push((score, e));
                }
            }
        }
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.truncate(limit);
        scored.into_iter().map(|(_, e)| e).collect()
    }

    /// Resolve a spoken query ("fire fox", "dolphin", "kate") to the best
    /// matching entry. Exact and prefix matches beat fuzzy matches.
    pub fn lookup(&self, query: &str) -> Option<&DesktopEntry> {
        let words = content_words(query);
        if words.is_empty() {
            return None;
        }
        let mut best: Option<(i64, &DesktopEntry)> = None;
        for entry in &self.entries {
            let targets: Vec<String> = entry
                .match_targets()
                .iter()
                .map(|t| t.to_lowercase())
                .collect();
            if let Some(score) = self.score(&targets, &words) {
                if best.is_none_or(|(previous, _)| score > previous) {
                    best = Some((score, entry));
                }
            }
        }
        best.map(|(_, entry)| entry)
    }

    fn score(&self, targets: &[String], words: &[String]) -> Option<i64> {
        // Subsequence scores alone let "kate" match "KDE Partition Manager".
        // Every content token must occur contiguously in at least one target.
        if words.is_empty()
            || !words
                .iter()
                .all(|word| targets.iter().any(|target| target.contains(word)))
        {
            return None;
        }
        let query = words.join(" ");
        let compact = words.concat();
        targets
            .iter()
            .map(|target| {
                if target == &query || target == &compact {
                    10_000
                } else if target.starts_with(&query) || target.starts_with(&compact) {
                    5_000
                } else {
                    // Consider every target, including keywords after a weak name
                    // hit. Token coverage also permits tokens split across targets.
                    self.matcher
                        .fuzzy_match(target, &query)
                        .into_iter()
                        .chain(self.matcher.fuzzy_match(target, &compact))
                        .max()
                        .unwrap_or(0)
                        .clamp(self.min_score, 4_999)
                }
            })
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an index over a throwaway applications dir.
    fn index_of(entries: &[(&str, &str)]) -> (PathBuf, DesktopIndex) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "parla-desktop-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let apps = root.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        for (id, name) in entries {
            std::fs::write(
                apps.join(id),
                format!("[Desktop Entry]\nType=Application\nName={name}\nExec=/bin/true\n"),
            )
            .unwrap();
        }
        let idx = DesktopIndex::from_dirs(std::slice::from_ref(&root));
        (root, idx)
    }

    #[test]
    fn xdg_inputs_are_data_directories() {
        let (root, _) = index_of(&[]);
        let home = root.join("home");
        let system = root.join("system");
        let custom = root.join("custom");
        for (data_dir, id, name) in [
            (home.join(".local/share"), "app.desktop", "User App"),
            (system.clone(), "app.desktop", "System App"),
            (
                home.join(".local/share/flatpak/exports/share"),
                "flatpak.desktop",
                "Flatpak App",
            ),
            (custom.clone(), "app.desktop", "Custom App"),
        ] {
            std::fs::create_dir_all(data_dir.join("applications")).unwrap();
            std::fs::write(
                data_dir.join("applications").join(id),
                format!("[Desktop Entry]\nType=Application\nName={name}\n"),
            )
            .unwrap();
        }
        let dirs = xdg_data_dirs(Some(&home), None, Some(system.clone().into_os_string()));
        assert_eq!(dirs[0], home.join(".local/share"));
        assert_eq!(
            dirs.last().unwrap(),
            &PathBuf::from("/var/lib/flatpak/exports/share")
        );
        let index = DesktopIndex::from_dirs(&dirs);
        assert_eq!(
            index
                .entries()
                .iter()
                .find(|entry| entry.id == "app.desktop")
                .unwrap()
                .name,
            "User App"
        );
        assert!(index
            .entries()
            .iter()
            .any(|entry| entry.id == "flatpak.desktop"));
        let dirs = xdg_data_dirs(
            Some(&home),
            Some(custom.clone()),
            Some(system.into_os_string()),
        );
        assert_eq!(dirs[0], custom);
        let index = DesktopIndex::from_dirs(&dirs);
        assert_eq!(
            index
                .entries()
                .iter()
                .find(|entry| entry.id == "app.desktop")
                .unwrap()
                .name,
            "Custom App"
        );
        assert_eq!(
            xdg_data_dirs(None, None, Some("".into()))[..2],
            [
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share")
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn normalize_strips_punctuation_and_case() {
        assert_eq!(normalize("Open Firefox, please!"), "open firefox please");
        assert_eq!(
            normalize("don't close Kate's window"),
            "don't close kate's window"
        );
        assert_eq!(normalize("  focus   firefox  "), "focus firefox");
    }

    #[test]
    fn shortlist_finds_the_app_inside_a_sentence() {
        let (root, idx) = index_of(&[
            ("firefox.desktop", "Firefox"),
            ("org.kde.kate.desktop", "Kate"),
            ("org.kde.dolphin.desktop", "Dolphin"),
        ]);

        // lookup() needs the name already isolated and fails on a sentence;
        // shortlist() is what lets a caller narrow before choosing.
        let names = |text: &str| -> Vec<String> {
            idx.shortlist(text, 8)
                .iter()
                .map(|e| e.name.clone())
                .collect()
        };
        assert!(names("could you bring up firefox for me").contains(&"Firefox".to_string()));
        assert!(names("shut down kate please").contains(&"Kate".to_string()));
        // Nothing app-like in the utterance: no candidates rather than a
        // bogus low-score hit.
        assert!(names("what is the weather tomorrow").is_empty());

        std::fs::remove_dir_all(&root).ok();
    }
}
