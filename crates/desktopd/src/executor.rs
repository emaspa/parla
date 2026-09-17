//! The executor: one implementation of the desktopd tool surface, called by
//! the fast-path router directly (Rust) and later by the MCP server (plan §1:
//! "one implementation, two callers").
//!
//! Every method returns a short human-readable result string — the daemon
//! uses it for notifications/TTS, the agent uses it as tool output.

use std::sync::Arc;

use parla_grammar::{DesktopIndex, Intent};
use tokio::task::spawn_blocking;

use crate::config::DesktopdConfig;
use crate::injector::{self, TextInjector};
use crate::tmuxctl::TmuxCtl;
use crate::windows::{Window, WindowCtl};

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

    /// Execute a parsed fast-path intent.
    ///
    /// NOTE: confirmation policy (plan §5) is the *daemon's* job — it must ask
    /// before calling this with an intent where `needs_confirmation()` is true.
    pub async fn execute(&self, intent: Intent) -> anyhow::Result<String> {
        match intent {
            Intent::LaunchApp { query } => self.launch_app(&query).await,
            Intent::OpenTerminal => self.open_terminal(None).await,
            Intent::FocusWindow { query } => self.focus_window(&query).await,
            Intent::CloseWindow { query } => self.close_window(query.as_deref()).await,
            Intent::MinimizeWindow { query } => self.window_op(query.as_deref(), "minimize").await,
            Intent::MaximizeWindow { query } => self.window_op(query.as_deref(), "maximize").await,
            Intent::VirtualDesktop { n } => crate::kwin::switch_to(n).await.map(|_| format!("desktop {n}")),
            Intent::VirtualDesktopRel { delta } => {
                crate::kwin::switch_rel(delta).await.map(|_| "switched desktop".to_string())
            }
            Intent::RunShortcut { component, action } => self.run_shortcut(&component, &action).await,
            Intent::KRunner { query } => crate::kwin::krunner_query(&query)
                .await
                .map(|_| format!("krunner: {query}")),
            Intent::StartClaude { model } => self.start_claude(model.as_deref()).await,
            Intent::ClaudeModel { model } => self.tmux().switch_model(&model).await
                .map(|_| format!("claude model -> {model}")),
            Intent::ClaudeTell { text } => self.tmux().send(&text).await
                .map(|_| "sent to claude".to_string()),
            Intent::ClaudeRead => {
                let tail = self.tmux().read_tail(40).await?;
                Ok(tail)
            }
            Intent::Notify { text } => crate::notify::notify("parla", &text)
                .await
                .map(|_| "notified".to_string()),
            Intent::Key { chord } => self.key(&chord).await,
        }
    }

    // ---- tool surface (also what the MCP server will expose in P3) ----

    pub async fn launch_app(&self, query: &str) -> anyhow::Result<String> {
        let entry = self.resolve_app(query)?;
        if self.cfg.focus_if_running {
            let stem = entry.id.trim_end_matches(".desktop");
            let class = stem.rsplit('.').next().unwrap_or(stem);
            if let Ok(w) = self.windows.find(class).await {
                if w.class.to_lowercase().contains(&class.to_lowercase()) {
                    self.windows.activate(&w.id).await?;
                    return Ok(format!("focused running {} ({})", entry.name, truncate(&w.title, 60)));
                }
            }
        }
        crate::launcher::launch_app(&entry, &self.cfg).await
    }

    /// The .desktop index, for callers that need to offer candidates rather
    /// than resolve a single query.
    pub fn desktop_index(&self) -> &DesktopIndex {
        &self.index
    }

    pub fn resolve_app(&self, query: &str) -> anyhow::Result<parla_grammar::DesktopEntry> {
        self.index
            .lookup(query)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no application matches {query:?}"))
    }

    pub async fn open_terminal(&self, command: Option<Vec<String>>) -> anyhow::Result<String> {
        crate::launcher::spawn_terminal(&self.cfg, command)?;
        Ok(format!("opened {}", self.cfg.terminal))
    }

    pub async fn focus_window(&self, query: &str) -> anyhow::Result<String> {
        let w = self.windows.find(query).await?;
        self.windows.activate(&w.id).await?;
        Ok(format!("focused {}", truncate(&w.title, 60)))
    }

    pub async fn close_window(&self, query: Option<&str>) -> anyhow::Result<String> {
        let w = self.target_window(query).await?;
        self.windows.close(&w.id).await?;
        Ok(format!("closed {}", truncate(&w.title, 60)))
    }

    pub async fn window_op(&self, query: Option<&str>, op: &str) -> anyhow::Result<String> {
        let w = self.target_window(query).await?;
        match op {
            "minimize" => self.windows.minimize(&w.id).await?,
            "maximize" => self.windows.maximize(&w.id).await?,
            other => anyhow::bail!("unknown window op {other}"),
        }
        Ok(format!("{op} {}", truncate(&w.title, 60)))
    }

    async fn target_window(&self, query: Option<&str>) -> anyhow::Result<Window> {
        match query {
            Some(q) if !q.trim().is_empty() && q != "window" => self.windows.find(q).await,
            _ => {
                let id = self.windows.active_id().await?;
                self.windows.window_info(&id).await
            }
        }
    }

    pub async fn run_shortcut(&self, component: &str, action: &str) -> anyhow::Result<String> {
        crate::shortcuts::invoke(component, action).await?;
        Ok(format!("ran shortcut {component}/{action}"))
    }

    pub async fn list_windows(&self) -> anyhow::Result<Vec<Window>> {
        self.windows.list().await
    }

    pub async fn start_claude(&self, model: Option<&str>) -> anyhow::Result<String> {
        let tmux = self.tmux();
        let outcome = tmux.ensure_claude_session(model).await?;
        // attach a terminal for viewing (idempotent: attaches to same session)
        self.open_terminal(Some(tmux.attach_command())).await?;
        Ok(match outcome {
            crate::tmuxctl::StartOutcome::Created => match model {
                Some(m) => format!("started claude ({m}) and attached terminal"),
                None => "started claude and attached terminal".to_string(),
            },
            crate::tmuxctl::StartOutcome::AlreadyRunning => {
                "claude already running; attached terminal".to_string()
            }
        })
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

    /// Send a key chord ("ctrl+s", "enter") to the focused window.
    pub async fn key(&self, chord: &str) -> anyhow::Result<String> {
        let injector = Arc::clone(&self.injector);
        let owned = chord.to_string();
        spawn_blocking(move || injector.key_chord(&owned))
            .await
            .map_err(|e| anyhow::anyhow!("injection task panicked: {e}"))??;
        Ok(format!("sent key {chord}"))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
