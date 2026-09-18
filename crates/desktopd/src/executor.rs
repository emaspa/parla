//! The executor: one implementation of the desktopd tool surface, called by
//! the fast-path router directly (Rust) and later by the MCP server (plan §1:
//! "one implementation, two callers").
//!
//! Resolution and action are separate steps: `resolve_target` finds a window
//! and the `*_window_id` verbs act on it, so a confirmation flow (or an MCP
//! caller) can look at what would happen before it happens. `execute` glues
//! the two together for a [`Command`] that was already confirmed.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;

use crate::command::{AppTarget, Command, WindowTarget};
use crate::config::DesktopdConfig;
use crate::desktop::{DesktopEntry, DesktopIndex};
use crate::injector::{self, TextInjector};
use crate::tmuxctl::TmuxCtl;
use crate::windows::{Window, WindowCtl};

/// What an action did, for notifications, TTS and tool output.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Outcome {
    /// Short human-readable result ("focused Inbox - Firefox").
    pub summary: String,
    /// The window acted on, when the action targeted one.
    pub window: Option<Window>,
    /// The .desktop id launched or focused, when the action targeted an app.
    pub entry_id: Option<String>,
}

impl Outcome {
    pub fn text(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            ..Default::default()
        }
    }

    pub fn on_window(summary: impl Into<String>, window: Window) -> Self {
        Self {
            summary: summary.into(),
            window: Some(window),
            entry_id: None,
        }
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)
    }
}

/// Something done to one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowOp {
    Focus,
    Close,
    Minimize,
    Maximize,
}

impl WindowOp {
    /// The imperative, for prompts ("close").
    pub fn verb(self) -> &'static str {
        match self {
            WindowOp::Focus => "focus",
            WindowOp::Close => "close",
            WindowOp::Minimize => "minimize",
            WindowOp::Maximize => "maximize",
        }
    }

    fn past_tense(self) -> &'static str {
        match self {
            WindowOp::Focus => "focused",
            WindowOp::Close => "closed",
            WindowOp::Minimize => "minimized",
            WindowOp::Maximize => "maximized",
        }
    }
}

/// Everything the judged path (and a confirmation prompt) needs to know
/// about the desktop, gathered in one place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopState {
    pub windows: Vec<Window>,
    /// 1-based.
    pub current_desktop: u32,
    pub desktop_count: u32,
    pub claude_running: bool,
    pub focused: Option<Window>,
}

pub struct Executor {
    cfg: DesktopdConfig,
    injector: Arc<dyn TextInjector>,
    windows: WindowCtl,
    index: DesktopIndex,
}

impl Executor {
    /// Build the executor: probes injectors in preference order and indexes
    /// .desktop entries. Fails only if no injector works at all.
    pub async fn new(cfg: DesktopdConfig) -> anyhow::Result<Self> {
        let injector = injector::select(&cfg.injectors, cfg.ydotool_socket.as_deref()).await?;
        let index = spawn_blocking(DesktopIndex::from_xdg)
            .await
            .map_err(|e| anyhow::anyhow!("desktop index task panicked: {e}"))?;
        Ok(Self {
            cfg,
            injector,
            windows: WindowCtl::new(),
            index,
        })
    }

    pub fn injector_name(&self) -> &'static str {
        self.injector.name()
    }

    pub fn tmux(&self) -> TmuxCtl {
        TmuxCtl::new(
            self.cfg.claude_tmux_session.clone(),
            self.cfg.claude_command.clone(),
        )
    }

    /// Execute a command; the summary line only.
    ///
    /// NOTE: confirmation policy (plan §5) is the *daemon's* job — it must ask
    /// before calling this with anything the user has to confirm.
    pub async fn execute(&self, command: Command) -> anyhow::Result<String> {
        self.execute_outcome(command).await.map(|o| o.summary)
    }

    /// Execute a command and report what it touched.
    pub async fn execute_outcome(&self, command: Command) -> anyhow::Result<Outcome> {
        Ok(match command {
            Command::LaunchApp { app } => match app {
                AppTarget::Query(query) => self.launch_app(&query).await?,
                AppTarget::Entry(id) => {
                    let entry = self
                        .index
                        .by_id(&id)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("no application with id {id:?}"))?;
                    self.launch_entry(entry).await?
                }
            },
            Command::OpenTerminal => self.open_terminal(None).await?,
            Command::Window { op, target } => self.window_op(&target, op).await?,
            Command::VirtualDesktop { n } => {
                crate::kwin::switch_to(n).await?;
                Outcome::text(format!("desktop {n}"))
            }
            Command::VirtualDesktopRel { delta } => {
                crate::kwin::switch_rel(delta).await?;
                Outcome::text("switched desktop")
            }
            Command::RunShortcut { component, action } => {
                self.run_shortcut(&component, &action).await?
            }
            Command::KRunner { query } => {
                crate::kwin::krunner_query(&query).await?;
                Outcome::text(format!("krunner: {query}"))
            }
            Command::StartClaude { model } => self.start_claude(model.as_deref()).await?,
            Command::ClaudeModel { model } => {
                self.tmux().switch_model(&model).await?;
                Outcome::text(format!("claude model -> {model}"))
            }
            Command::ClaudeTell { text } => {
                self.tmux().send(&text).await?;
                Outcome::text("sent to claude")
            }
            Command::ClaudeRead => Outcome::text(self.tmux().read_tail(40).await?),
            Command::Notify { text } => {
                crate::notify::notify("parla", &text).await?;
                Outcome::text("notified")
            }
            Command::Key { chord } => self.key(&chord).await?,
        })
    }

    // ---- state ----------------------------------------------------------

    /// One snapshot of what the desktop looks like right now. Each part is
    /// fetched once; a part that cannot be read degrades to empty/false and
    /// is logged, so a dead KWin script engine does not take the judged
    /// path down with it.
    pub async fn snapshot(&self) -> DesktopState {
        let tmux = self.tmux();
        let (windows, desktops, claude) = tokio::join!(
            self.windows.list_all(),
            crate::kwin::desktop_position(),
            tmux.session_state(),
        );
        let all = windows.unwrap_or_else(|e| {
            tracing::warn!("window list unavailable: {e:#}");
            Vec::new()
        });
        let focused = all.iter().find(|w| w.active).cloned();
        let windows = all
            .into_iter()
            .filter(|w| w.normal && !w.title.is_empty())
            .collect();
        let (current_desktop, desktop_count) = desktops.unwrap_or_else(|e| {
            tracing::warn!("virtual desktops unavailable: {e:#}");
            (1, 1)
        });
        let claude_running = claude.unwrap_or_else(|e| {
            tracing::warn!("tmux unavailable: {e:#}");
            false
        });
        DesktopState {
            windows,
            current_desktop,
            desktop_count,
            claude_running,
            focused,
        }
    }

    pub async fn list_windows(&self) -> anyhow::Result<Vec<Window>> {
        self.windows.list().await
    }

    /// The window that has focus, or None when none does.
    pub async fn active_window(&self) -> anyhow::Result<Option<Window>> {
        self.windows.active().await
    }

    /// The .desktop index, for callers that need to offer candidates rather
    /// than resolve a single query.
    pub fn desktop_index(&self) -> &DesktopIndex {
        &self.index
    }

    pub fn resolve_app(&self, query: &str) -> anyhow::Result<DesktopEntry> {
        self.index
            .lookup(query)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no application matches {query:?}"))
    }

    /// Resolve a spoken window query without touching anything. Fails with
    /// `WindowError::Ambiguous` (carrying the candidates) when two windows
    /// of different apps are equally plausible.
    pub async fn resolve_window(&self, query: &str) -> anyhow::Result<Window> {
        self.windows.find(query).await
    }

    /// The window a target denotes right now, without touching it. An id
    /// that is no longer open is an error, not a stale window.
    pub async fn resolve_target(&self, target: &WindowTarget) -> anyhow::Result<Window> {
        match target {
            WindowTarget::Query(q) => self.resolve_window(q).await,
            WindowTarget::Id(id) => self.windows.window_info(id).await,
            WindowTarget::Focused => self
                .windows
                .active()
                .await?
                .ok_or_else(|| anyhow::anyhow!("no window is focused")),
        }
    }

    // ---- tool surface (also what the MCP server will expose in P3) ----

    pub async fn launch_app(&self, query: &str) -> anyhow::Result<Outcome> {
        let entry = self.resolve_app(query)?;
        self.launch_entry(entry).await
    }

    /// Launch (or, with `focus_if_running`, focus) one indexed entry.
    pub async fn launch_entry(&self, entry: DesktopEntry) -> anyhow::Result<Outcome> {
        if self.cfg.focus_if_running {
            let stem = entry.id.trim_end_matches(".desktop");
            let class = stem.rsplit('.').next().unwrap_or(stem).to_lowercase();
            let running = self.windows.list().await.unwrap_or_default();
            let scored: Vec<(i64, Window)> = running
                .into_iter()
                .filter(|w| w.class.to_lowercase().contains(&class))
                .map(|w| (0, w))
                .collect();
            if let Ok(w) = crate::windows::pick(&class, scored) {
                self.windows.activate(&w.id).await?;
                return Ok(Outcome {
                    summary: format!(
                        "focused running {} ({})",
                        entry.name,
                        truncate(&w.title, 60)
                    ),
                    window: Some(w),
                    entry_id: Some(entry.id.clone()),
                });
            }
        }
        let summary = crate::launcher::launch_app(&entry, &self.cfg).await?;
        Ok(Outcome {
            summary,
            window: None,
            entry_id: Some(entry.id),
        })
    }

    pub async fn open_terminal(&self, command: Option<Vec<String>>) -> anyhow::Result<Outcome> {
        crate::launcher::spawn_terminal(&self.cfg, command)?;
        Ok(Outcome::text(format!("opened {}", self.cfg.terminal)))
    }

    /// Resolve `target` and apply `op`.
    pub async fn window_op(&self, target: &WindowTarget, op: WindowOp) -> anyhow::Result<Outcome> {
        let w = self.resolve_target(target).await?;
        self.window_op_id(&w.id, op).await?;
        Ok(Outcome::on_window(
            format!("{} {}", op.past_tense(), truncate(&w.title, 60)),
            w,
        ))
    }

    /// Apply `op` to a window already resolved by id.
    pub async fn window_op_id(&self, id: &str, op: WindowOp) -> anyhow::Result<()> {
        match op {
            WindowOp::Focus => self.windows.activate(id).await,
            WindowOp::Close => self.windows.close(id).await,
            WindowOp::Minimize => self.windows.minimize(id).await,
            WindowOp::Maximize => self.windows.maximize(id).await,
        }
    }

    pub async fn focus_window(&self, query: &str) -> anyhow::Result<Outcome> {
        self.window_op(&WindowTarget::Query(query.into()), WindowOp::Focus)
            .await
    }

    pub async fn close_window(&self, query: Option<&str>) -> anyhow::Result<Outcome> {
        self.window_op(&WindowTarget::from_query(query), WindowOp::Close)
            .await
    }

    pub async fn focus_window_id(&self, id: &str) -> anyhow::Result<()> {
        self.window_op_id(id, WindowOp::Focus).await
    }

    pub async fn close_window_id(&self, id: &str) -> anyhow::Result<()> {
        self.window_op_id(id, WindowOp::Close).await
    }

    pub async fn minimize_window_id(&self, id: &str) -> anyhow::Result<()> {
        self.window_op_id(id, WindowOp::Minimize).await
    }

    pub async fn maximize_window_id(&self, id: &str) -> anyhow::Result<()> {
        self.window_op_id(id, WindowOp::Maximize).await
    }

    pub async fn run_shortcut(&self, component: &str, action: &str) -> anyhow::Result<Outcome> {
        crate::shortcuts::invoke(component, action).await?;
        Ok(Outcome::text(format!("ran shortcut {component}/{action}")))
    }

    pub async fn start_claude(&self, model: Option<&str>) -> anyhow::Result<Outcome> {
        let tmux = self.tmux();
        let outcome = tmux.ensure_claude_session(model).await?;
        // attach a terminal for viewing (idempotent: attaches to same session)
        self.open_terminal(Some(tmux.attach_command())).await?;
        Ok(Outcome::text(match outcome {
            crate::tmuxctl::StartOutcome::Created => match model {
                Some(m) => format!("started claude ({m}) and attached terminal"),
                None => "started claude and attached terminal".to_string(),
            },
            crate::tmuxctl::StartOutcome::AlreadyRunning => {
                "claude already running; attached terminal".to_string()
            }
        }))
    }

    /// Type literal text into the focused window (dictation commit path).
    pub async fn type_text(&self, text: &str) -> anyhow::Result<String> {
        let injector = Arc::clone(&self.injector);
        let name = self.injector.name();
        let owned = text.to_string();
        let n = owned.chars().count();
        spawn_blocking(move || injector.type_text(&owned))
            .await
            .map_err(|e| anyhow::anyhow!("injection task panicked: {e}"))??;
        Ok(format!("typed {n} chars via {name}"))
    }

    /// Delete `n` characters before the cursor in the focused window, as
    /// `n` Backspace presses. Used to take back or replace a dictation.
    pub async fn backspace(&self, n: usize) -> anyhow::Result<()> {
        if n == 0 {
            return Ok(());
        }
        let injector = Arc::clone(&self.injector);
        spawn_blocking(move || {
            for _ in 0..n {
                injector.key_chord("backspace")?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("injection task panicked: {e}"))??;
        Ok(())
    }

    /// Send a key chord ("ctrl+s", "enter") to the focused window.
    pub async fn key(&self, chord: &str) -> anyhow::Result<Outcome> {
        let injector = Arc::clone(&self.injector);
        let owned = chord.to_string();
        spawn_blocking(move || injector.key_chord(&owned))
            .await
            .map_err(|e| anyhow::anyhow!("injection task panicked: {e}"))??;
        Ok(Outcome::text(format!("sent key {chord}")))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
