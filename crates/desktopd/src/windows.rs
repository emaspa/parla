//! Window control via kdotool (generates KWin scripts over D-Bus; works on
//! Plasma 6 Wayland + X11). Window ids are KWin UUIDs like `{824b81c2-...}`.
//!
//! Listing is one inline KWin script that returns every window as JSON, so
//! a query costs one process, not one per window. Matching happens here in
//! Rust; user text never reaches a KWin script. The one place it could
//! (`search_ids`) escapes it for both the regex and the template literal
//! kdotool pastes it into.

use std::time::{Duration, Instant};

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use serde::{Deserialize, Serialize};

use crate::proc::Cmd;

/// Fuzzy scores below this are noise. Same bar as the grammar's app lookup.
pub const MIN_SCORE: i64 = 40;
/// Two candidates of different apps closer than this are ambiguous.
pub const TIE_MARGIN: i64 = 10;
/// Exact title/class match.
const SCORE_EXACT: i64 = 1000;
/// Class begins with the query ("kate" for "org.kde.kate" does not, "kate" does).
const SCORE_PREFIX: i64 = 500;

const FOCUS_VERIFY_BUDGET: Duration = Duration::from_millis(1000);
const FOCUS_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Window {
    pub id: String,
    /// The caption shown in the titlebar.
    #[serde(alias = "caption")]
    pub title: String,
    /// resourceClass (the WM_CLASS class / Wayland app id).
    #[serde(alias = "resourceClass")]
    pub class: String,
    /// resourceName (the WM_CLASS instance).
    #[serde(default, alias = "resourceName")]
    pub resource_name: String,
    /// 1-based virtual desktop, -1 when on all desktops or none.
    #[serde(default = "minus_one")]
    pub desktop: i32,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub minimized: bool,
    /// Position in KWin's stacking order; higher is nearer the top, so the
    /// most recently used window of an app has the largest value.
    #[serde(default = "minus_one", alias = "stack")]
    pub stacking: i32,
    #[serde(default)]
    pub pid: i64,
    /// normalWindow || dialog: what a user would call "a window".
    #[serde(default = "yes", alias = "normal")]
    pub normal: bool,
}

fn minus_one() -> i32 {
    -1
}
fn yes() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    #[error("no window matches {query:?}")]
    NotFound { query: String },
    /// More than one window is a plausible target. `candidates` is sorted
    /// best-first; a confirmation flow can offer them.
    #[error("{query:?} could mean {}", describe(candidates))]
    Ambiguous {
        query: String,
        candidates: Vec<Window>,
    },
}

fn describe(c: &[Window]) -> String {
    c.iter()
        .map(|w| format!("{} [{}]", short(&w.title), w.class))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn short(s: &str) -> String {
    if s.chars().count() <= 40 {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(39).collect::<String>())
    }
}

/// Escape a string for use inside kdotool's search PATTERN. kdotool pastes
/// it verbatim into `new RegExp(String.raw`...`)` in a KWin script, so both
/// regex metacharacters and everything that ends a template literal (backtick,
/// `${`) must be neutralized. Line breaks and other control characters have
/// no business in a window title query and are refused.
pub fn escape_regex(s: &str) -> anyhow::Result<String> {
    if let Some(c) = s.chars().find(|c| c.is_control()) {
        anyhow::bail!("window query contains control character {c:?}");
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            // regex metacharacters: a literal match is what a spoken query means
            '\\' | '^' | '$' | '.' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'
            | '/' => {
                out.push('\\');
                out.push(c);
            }
            // String.raw keeps backslashes, so a backtick cannot be escaped
            // in place; spell it as a regex hex escape instead
            '`' => out.push_str("\\x60"),
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// KWin window ids are `{uuid}`. Refuse anything else before it reaches a
/// script argument.
fn validate_id(id: &str) -> anyhow::Result<()> {
    let inner = id
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| anyhow::anyhow!("window id {id:?} is not a {{uuid}}"))?;
    anyhow::ensure!(
        inner.len() == 36 && inner.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
        "window id {id:?} is not a {{uuid}}"
    );
    Ok(())
}

/// Inline KWin script: every window as one JSON array on one result line.
/// No caller input is interpolated.
const LIST_SCRIPT: &str = r#"
function run() {
    var out = [];
    var stack = workspace.stackingOrder;
    var list = workspace.windowList();
    for (var i = 0; i < list.length; i++) {
        var w = list[i];
        out.push({
            id: String(w.internalId),
            caption: String(w.caption),
            resourceClass: String(w.resourceClass),
            resourceName: String(w.resourceName),
            desktop: (w.onAllDesktops || w.desktops.length == 0) ? -1 : w.desktops[0].x11DesktopNumber,
            active: !!w.active,
            minimized: !!w.minimized,
            stack: stack.indexOf(w),
            pid: w.pid,
            normal: !!(w.normalWindow || w.dialog)
        });
    }
    output_result(JSON.stringify(out));
}
run();
"#;

async fn kdotool(args: &[&str]) -> anyhow::Result<String> {
    Ok(Cmd::new("kdotool").args(args.iter().copied()).run().await?)
}

/// Parse the stdout of the list script: kdotool prints one result line per
/// `output_result`; ours is the line that is a JSON array.
pub fn parse_window_list(stdout: &str) -> anyhow::Result<Vec<Window>> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with('['))
        .ok_or_else(|| anyhow::anyhow!("kdotool kwinscript returned no JSON line: {stdout:?}"))?;
    let wins: Vec<Window> = serde_json::from_str(line)
        .map_err(|e| anyhow::anyhow!("window list JSON: {e}"))?;
    Ok(wins)
}

pub struct WindowCtl {
    matcher: SkimMatcherV2,
}

impl WindowCtl {
    pub fn new() -> Self {
        Self {
            matcher: SkimMatcherV2::default(),
        }
    }

    pub async fn active_id(&self) -> anyhow::Result<String> {
        let id = kdotool(&["getactivewindow"]).await?.trim().to_string();
        anyhow::ensure!(id.starts_with('{'), "no active window (got {id:?})");
        Ok(id)
    }

    /// Every KWin window, including panels and untitled helpers. One process.
    pub async fn list_all(&self) -> anyhow::Result<Vec<Window>> {
        let out = kdotool(&["kwinscript", "--inline", LIST_SCRIPT]).await?;
        parse_window_list(&out)
    }

    /// The windows a user would name: normal windows and dialogs with a title.
    pub async fn list(&self) -> anyhow::Result<Vec<Window>> {
        Ok(self
            .list_all()
            .await?
            .into_iter()
            .filter(|w| w.normal && !w.title.is_empty())
            .collect())
    }

    /// The focused window, if any.
    pub async fn active(&self) -> anyhow::Result<Option<Window>> {
        Ok(self.list_all().await?.into_iter().find(|w| w.active))
    }

    /// Look one window up by id. An unknown id is an error, not an empty window.
    pub async fn window_info(&self, id: &str) -> anyhow::Result<Window> {
        validate_id(id)?;
        self.list_all()
            .await?
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| anyhow::anyhow!("no window with id {id}"))
    }

    /// Ids of windows whose title/class/classname match `query` literally
    /// (case-insensitive). Goes through kdotool's regex search with the
    /// query escaped; prefer `find`, which needs no script interpolation.
    pub async fn search_ids(&self, query: &str) -> anyhow::Result<Vec<String>> {
        let pattern = escape_regex(query)?;
        let out = kdotool(&["search", "--title", "--class", "--classname", &pattern]).await?;
        Ok(out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| l.starts_with('{'))
            .collect())
    }

    /// Score one window against a lowercased query; None when it is not a
    /// candidate at all.
    fn score(&self, w: &Window, ql: &str) -> Option<i64> {
        let title = w.title.to_lowercase();
        let class = w.class.to_lowercase();
        let name = w.resource_name.to_lowercase();
        if class == ql || title == ql || name == ql {
            return Some(SCORE_EXACT);
        }
        if class.starts_with(ql) || name.starts_with(ql) {
            return Some(SCORE_PREFIX);
        }
        let s = self
            .matcher
            .fuzzy_match(&title, ql)
            .unwrap_or(0)
            .max(self.matcher.fuzzy_match(&class, ql).unwrap_or(0));
        (s >= MIN_SCORE).then_some(s)
    }

    /// Resolve a spoken query ("fire fox", "dolphin", "parla main.rs") to
    /// one window. Errors are `WindowError::NotFound` or `Ambiguous`.
    pub async fn find(&self, query: &str) -> anyhow::Result<Window> {
        let q = query.trim();
        anyhow::ensure!(!q.is_empty(), "empty window query");
        let ql = q.to_lowercase();
        let scored: Vec<(i64, Window)> = self
            .list()
            .await?
            .into_iter()
            .filter_map(|w| self.score(&w, &ql).map(|s| (s, w)))
            .collect();
        Ok(pick(q, scored)?)
    }

    pub async fn activate(&self, id: &str) -> anyhow::Result<()> {
        validate_id(id)?;
        kdotool(&["windowactivate", id]).await?;
        // plan §5: always verify focus before typing; mismatch is a hard
        // error. Focus lands asynchronously (Wayland focus-stealing rules,
        // compositor round trip), so poll instead of guessing one delay.
        let deadline = Instant::now() + FOCUS_VERIFY_BUDGET;
        let mut active = String::new();
        loop {
            tokio::time::sleep(FOCUS_POLL).await;
            active = self.active_id().await.unwrap_or(active);
            if active == id {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "focus verification failed: activated {id} but {active} is focused"
                );
            }
        }
    }

    pub async fn close(&self, id: &str) -> anyhow::Result<()> {
        validate_id(id)?;
        kdotool(&["windowclose", id]).await.map(|_| ())
    }

    pub async fn minimize(&self, id: &str) -> anyhow::Result<()> {
        validate_id(id)?;
        kdotool(&["windowminimize", id]).await.map(|_| ())
    }

    /// kdotool has no maximize verb; go through its inline KWin script mode.
    /// Falls back to windowsize+windowmove if the scripting call fails.
    pub async fn maximize(&self, id: &str) -> anyhow::Result<()> {
        validate_id(id)?;
        let uuid = id.trim_matches(|c| c == '{' || c == '}');
        let js = format!(
            r#"
const id = "{uuid}";
for (const w of workspace.windowList()) {{
    if (String(w.internalId) === id) {{
        w.frameGeometry = workspace.clientArea(KWin.FullScreenArea, w);
    }}
}}
"#
        );
        match kdotool(&["kwinscript", "--inline", &js]).await {
            Ok(_) => return Ok(()),
            Err(e) => tracing::debug!("maximize via kwinscript failed, falling back: {e:#}"),
        }
        kdotool(&["windowsize", id, "100%", "100%"]).await?;
        kdotool(&["windowmove", id, "0", "0"]).await.map(|_| ())
    }
}

/// Choose among scored candidates. Ordering is deterministic: score, then
/// stacking order (top of stack = most recently used), then id. Two windows
/// of different apps within `TIE_MARGIN` of each other are ambiguous and
/// come back as an error carrying both; two windows of the same app are
/// settled by stacking order, since "focus kate" with two Kate windows means
/// the one used last.
pub fn pick(query: &str, mut scored: Vec<(i64, Window)>) -> Result<Window, WindowError> {
    scored.sort_by(|(sa, a), (sb, b)| {
        sb.cmp(sa)
            .then(b.stacking.cmp(&a.stacking))
            .then(a.id.cmp(&b.id))
    });
    let mut it = scored.into_iter();
    let Some((top_score, top)) = it.next() else {
        return Err(WindowError::NotFound {
            query: query.to_string(),
        });
    };
    let rivals: Vec<Window> = it
        .take_while(|(s, _)| top_score - *s <= TIE_MARGIN)
        .map(|(_, w)| w)
        .filter(|w| !w.class.eq_ignore_ascii_case(&top.class))
        .collect();
    if rivals.is_empty() {
        return Ok(top);
    }
    let mut candidates = vec![top];
    candidates.extend(rivals);
    Err(WindowError::Ambiguous {
        query: query.to_string(),
        candidates,
    })
}

impl Default for WindowCtl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: &str, title: &str, class: &str, stacking: i32) -> Window {
        Window {
            id: format!("{{{id}}}"),
            title: title.into(),
            class: class.into(),
            resource_name: class.into(),
            desktop: 1,
            active: false,
            minimized: false,
            stacking,
            pid: 1,
            normal: true,
        }
    }

    #[test]
    fn escape_neutralizes_template_and_regex_breakouts() {
        let p = escape_regex("a`b${c}\"d.e").unwrap();
        assert!(!p.contains('`'), "{p}");
        assert!(!p.contains("${"), "{p}");
        assert_eq!(p, r#"a\x60b\$\{c\}"d\.e"#);
        // the pattern must still match the literal text as a regex
        assert!(escape_regex("\n").is_err());
        assert!(escape_regex("a\rb").is_err());
        assert_eq!(escape_regex("plain title").unwrap(), "plain title");
    }

    #[test]
    fn list_json_fixture_parses() {
        let out = "debug: STEP kwinscript\n[{\"id\":\"{ce24dd73-7292-48a8-9712-0508b4edea3d}\",\"caption\":\"\",\"resourceClass\":\"plasmashell\",\"resourceName\":\"plasmashell\",\"desktop\":-1,\"active\":false,\"minimized\":false,\"normal\":false,\"stack\":4,\"pid\":4225},{\"id\":\"{177d8a47-f0fc-402e-a747-da4b6da4714b}\",\"caption\":\"Inbox - Mozilla Firefox\",\"resourceClass\":\"firefox\",\"resourceName\":\"Navigator\",\"desktop\":1,\"active\":true,\"minimized\":false,\"normal\":true,\"stack\":17,\"pid\":4767}]\n";
        let wins = parse_window_list(out).unwrap();
        assert_eq!(wins.len(), 2);
        let ff = &wins[1];
        assert_eq!(ff.id, "{177d8a47-f0fc-402e-a747-da4b6da4714b}");
        assert_eq!(ff.title, "Inbox - Mozilla Firefox");
        assert_eq!(ff.class, "firefox");
        assert_eq!(ff.resource_name, "Navigator");
        assert_eq!(ff.desktop, 1);
        assert!(ff.active && ff.normal);
        assert_eq!(ff.stacking, 17);
        assert!(!wins[0].normal);
        assert!(parse_window_list("no json here").is_err());
    }

    #[test]
    fn pick_prefers_top_of_stack_within_one_app() {
        let scored = vec![
            (500, win("a", "old.rs - Kate", "kate", 3)),
            (500, win("b", "new.rs - Kate", "kate", 9)),
        ];
        assert_eq!(pick("kate", scored).unwrap().id, "{b}");
    }

    #[test]
    fn pick_reports_ambiguity_between_apps() {
        let scored = vec![
            (60, win("a", "Kate Bush - Firefox", "firefox", 3)),
            (55, win("b", "notes.txt - Kate", "kate", 1)),
        ];
        match pick("kate", scored).unwrap_err() {
            WindowError::Ambiguous { candidates, .. } => {
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates[0].id, "{a}");
            }
            other => panic!("expected ambiguity, got {other}"),
        }
    }

    #[test]
    fn pick_clear_winner_and_not_found() {
        let scored = vec![
            (1000, win("a", "Dolphin", "dolphin", 1)),
            (60, win("b", "Dolphins - Firefox", "firefox", 5)),
        ];
        assert_eq!(pick("dolphin", scored).unwrap().id, "{a}");
        assert!(matches!(
            pick("nothing", vec![]).unwrap_err(),
            WindowError::NotFound { .. }
        ));
    }

    #[test]
    fn score_threshold_drops_noise() {
        let ctl = WindowCtl::new();
        let w = win("a", "Some unrelated editor", "code", 1);
        assert_eq!(ctl.score(&w, "kate"), None);
        assert_eq!(ctl.score(&w, "code"), Some(SCORE_EXACT));
        assert_eq!(ctl.score(&w, "co"), Some(SCORE_PREFIX));
    }

    #[test]
    fn ids_are_validated() {
        assert!(validate_id("{177d8a47-f0fc-402e-a747-da4b6da4714b}").is_ok());
        assert!(validate_id("177d8a47").is_err());
        assert!(validate_id("{\"; evil}").is_err());
    }
}
