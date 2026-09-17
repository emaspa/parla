use std::path::{Path, PathBuf};

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use unicase::UniCase;

/// A parsed .desktop application entry.
#[derive(Debug, Clone)]
pub struct DesktopEntry {
    /// Desktop id, e.g. `org.kde.dolphin.desktop` (path relative to the
    /// applications dir, as understood by kioclient/gtk-launch).
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
        let mut dirs: Vec<PathBuf> = Vec::new();
        let home = std::env::var("HOME").map(PathBuf::from).ok();
        if let Some(h) = &home {
            dirs.push(h.join(".local/share/applications"));
        }
        if let Ok(xdg) = std::env::var("XDG_DATA_DIRS") {
            dirs.extend(xdg.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
        } else {
            dirs.push(PathBuf::from("/usr/local/share"));
            dirs.push(PathBuf::from("/usr/share"));
        }
        if let Some(h) = &home {
            // flatpak exports
            dirs.push(h.join(".local/share/flatpak/exports/share/applications"));
        }
        dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
        Self::from_dirs(&dirs)
    }

    pub fn from_dirs(data_dirs: &[PathBuf]) -> Self {
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for dir in data_dirs {
            Self::load_dir(&dir.join("applications"), &mut entries, &mut seen);
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
        out: &mut Vec<DesktopEntry>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::load_dir(&path, out, seen);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // first dir in XDG order wins for duplicate ids
            if !seen.insert(name.to_string()) {
                continue;
            }
            if let Some(entry) = Self::parse(&path, name) {
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
        if hidden {
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

    /// Plausible entries for a whole utterance, best first.
    ///
    /// [`Self::lookup`] needs an already-isolated app name; this takes the raw
    /// words and scores every entry against each of them, so "bring up fire
    /// fox for me" still surfaces Firefox. Callers that cannot isolate the
    /// name themselves use this to narrow the field before choosing.
    pub fn shortlist(&self, text: &str, limit: usize) -> Vec<&DesktopEntry> {
        let words: Vec<String> = text
            .split_whitespace()
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
            })
            .filter(|w| w.len() >= 3)
            .collect();
        if words.is_empty() {
            return Vec::new();
        }

        // Probe with each content word, plus the whole utterance so multi-word
        // names ("visual studio code") can still win.
        let mut probes = words;
        probes.push(text.to_lowercase());

        let mut scored: Vec<(i64, &DesktopEntry)> = Vec::new();
        for e in &self.entries {
            let targets: Vec<String> = e.match_targets().iter().map(|t| t.to_lowercase()).collect();
            let best = targets
                .iter()
                .flat_map(|t| probes.iter().filter_map(|p| self.matcher.fuzzy_match(t, p)))
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
        let q = UniCase::new(query.trim().to_lowercase());
        let qs = q.as_ref();

        // 1. exact name / generic name / id stem
        if let Some(e) = self.entries.iter().find(|e| {
            UniCase::new(e.name.to_lowercase()) == q
                || e.generic_name
                    .as_ref()
                    .is_some_and(|g| UniCase::new(g.to_lowercase()) == q)
        }) {
            return Some(e);
        }

        // 2. prefix match on name
        if let Some(e) = self
            .entries
            .iter()
            .find(|e| e.name.to_lowercase().starts_with(qs))
        {
            return Some(e);
        }

        // 3. fuzzy over all match targets, best score above threshold wins
        let mut best: Option<(i64, &DesktopEntry)> = None;
        for e in &self.entries {
            for target in e.match_targets() {
                if let Some(score) = self.matcher.fuzzy_match(&target.to_lowercase(), qs) {
                    if score >= self.min_score && best.is_none_or(|(bs, _)| score > bs) {
                        best = Some((score, e));
                    }
                    break; // first matching target per entry is enough
                }
            }
        }
        best.map(|(_, e)| {
            tracing::debug!("fuzzy matched {:?} -> {}", query, e.id);
            e
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an index over a throwaway applications dir.
    fn index_of(entries: &[(&str, &str)]) -> (PathBuf, DesktopIndex) {
        let root = std::env::temp_dir().join(format!("parla-desktop-test-{}", std::process::id()));
        let apps = root.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        for (id, name) in entries {
            std::fs::write(
                apps.join(id),
                format!("[Desktop Entry]\nType=Application\nName={name}\nExec=/bin/true\n"),
            )
            .unwrap();
        }
        let idx = DesktopIndex::from_dirs(&[root.clone()]);
        (root, idx)
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
