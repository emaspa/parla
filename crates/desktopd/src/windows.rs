//! Window control via kdotool (generates KWin scripts over D-Bus; works on
//! Plasma 6 Wayland + X11). Window ids are KWin UUIDs like `{824b81c2-...}`.

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Window {
    pub id: String,
    pub title: String,
    pub class: String,
}

pub struct WindowCtl {
    matcher: SkimMatcherV2,
}

/// Escape a string for use as a kdotool (JS RegExp) pattern.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn kdotool(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("kdotool").args(args).output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "kdotool {} failed: {}",
            args.first().copied().unwrap_or("?"),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

    pub async fn window_info(&self, id: &str) -> anyhow::Result<Window> {
        let title = kdotool(&["getwindowname", id]).await.unwrap_or_default();
        let class = kdotool(&["getwindowclassname", id])
            .await
            .unwrap_or_default();
        Ok(Window {
            id: id.to_string(),
            title: title.trim().to_string(),
            class: class.trim().to_string(),
        })
    }

    pub async fn search_ids(&self, regex: &str) -> anyhow::Result<Vec<String>> {
        let out = kdotool(&["search", "--title", "--class", "--classname", regex]).await?;
        Ok(out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| l.starts_with('{'))
            .collect())
    }

    /// All windows with titles (kdotool search ".") plus their metadata.
    pub async fn list(&self) -> anyhow::Result<Vec<Window>> {
        let ids = self.search_ids(".").await?;
        let mut wins = Vec::with_capacity(ids.len());
        for id in ids {
            if let Ok(w) = self.window_info(&id).await {
                if !w.title.is_empty() {
                    wins.push(w);
                }
            }
        }
        Ok(wins)
    }

    /// Resolve a spoken query ("fire fox", "dolphin", "parla main.rs") to the
    /// best matching window: regex search first, fuzzy over title+class.
    pub async fn find(&self, query: &str) -> anyhow::Result<Window> {
        let q = query.trim();
        anyhow::ensure!(!q.is_empty(), "empty window query");

        // Candidates: windows whose title/class match the escaped query, else all.
        let ids = self.search_ids(&escape_regex(q)).await.unwrap_or_default();
        let candidates = if ids.is_empty() { self.list().await? } else {
            let mut v = Vec::new();
            for id in ids {
                if let Ok(w) = self.window_info(&id).await {
                    v.push(w);
                }
            }
            v
        };
        anyhow::ensure!(!candidates.is_empty(), "no windows found for {query:?}");

        let ql = q.to_lowercase();
        // exact/prefix on class or title first
        if let Some(w) = candidates.iter().find(|w| {
            w.class.eq_ignore_ascii_case(&ql) || w.title.eq_ignore_ascii_case(&ql)
        }) {
            return Ok(w.clone());
        }
        if let Some(w) = candidates
            .iter()
            .find(|w| w.class.to_lowercase().starts_with(&ql))
        {
            return Ok(w.clone());
        }
        // fuzzy: title and class scored separately, best wins
        let mut best: Option<(i64, &Window)> = None;
        for w in &candidates {
            let score = self
                .matcher
                .fuzzy_match(&w.title.to_lowercase(), &ql)
                .unwrap_or(0)
                .max(
                    self.matcher
                        .fuzzy_match(&w.class.to_lowercase(), &ql)
                        .unwrap_or(0),
                );
            if score > 0 && best.is_none_or(|(bs, _)| score > bs) {
                best = Some((score, w));
            }
        }
        best.map(|(_, w)| w.clone())
            .ok_or_else(|| anyhow::anyhow!("no window matches {query:?}"))
    }

    pub async fn activate(&self, id: &str) -> anyhow::Result<()> {
        kdotool(&["windowactivate", id]).await?;
        // plan §5: always verify focus before typing; mismatch is a hard error
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let active = self.active_id().await?;
        anyhow::ensure!(
            active == id,
            "focus verification failed: activated {id} but {active} is focused"
        );
        Ok(())
    }

    pub async fn close(&self, id: &str) -> anyhow::Result<()> {
        kdotool(&["windowclose", id]).await.map(|_| ())
    }

    pub async fn minimize(&self, id: &str) -> anyhow::Result<()> {
        kdotool(&["windowminimize", id]).await.map(|_| ())
    }

    /// kdotool has no maximize verb; go through its inline KWin script mode.
    /// Falls back to windowsize+windowmove if the scripting call fails.
    pub async fn maximize(&self, id: &str) -> anyhow::Result<()> {
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
        if kdotool(&["kwinscript", "--inline", &js]).await.is_ok() {
            return Ok(());
        }
        kdotool(&["windowsize", id, "100%", "100%"]).await?;
        kdotool(&["windowmove", id, "0", "0"]).await.map(|_| ())
    }
}

impl Default for WindowCtl {
    fn default() -> Self {
        Self::new()
    }
}
