//! Text/key injection with automatic fallback (plan §2: EIS first, ydotool
//! fallback; §Q4: probe at startup, log which injector is active, never let
//! injection die silently).

pub mod eis;
pub mod ydotool;

/// Injects text and key chords into the focused window.
///
/// Implementations must be cheap to call (the daemon calls them on every
/// dictation commit) and must fail loudly: undelivered keystrokes are a hard
/// error, never a silent drop.
pub trait TextInjector: Send + Sync {
    /// Stable name for logging/config ("eis", "ydotool").
    fn name(&self) -> &'static str;

    /// Type literal text into the focused window.
    fn type_text(&self, text: &str) -> anyhow::Result<()>;

    /// Send a key chord like "ctrl+s" or "enter".
    fn key_chord(&self, chord: &str) -> anyhow::Result<()>;

    /// Check availability right now (used at startup and for health checks).
    fn probe(&self) -> anyhow::Result<()>;
}

/// Pick the first injector whose probe succeeds, in preference order.
pub async fn select(
    prefs: &[String],
    ydotool_socket: Option<&str>,
) -> anyhow::Result<std::sync::Arc<dyn TextInjector>> {
    for name in prefs {
        let candidate: Option<std::sync::Arc<dyn TextInjector>> = match name.as_str() {
            // EIS setup is blocking (handshake + RESUMED wait with retries);
            // keep it off the async runtime
            "eis" => match tokio::task::spawn_blocking(eis::EisInjector::new).await {
                Ok(Ok(i)) => Some(std::sync::Arc::new(i)),
                Ok(Err(e)) => {
                    tracing::warn!("EIS injector unavailable: {e:#}");
                    None
                }
                Err(e) => {
                    tracing::warn!("EIS injector task panicked: {e}");
                    None
                }
            },
            "ydotool" => Some(std::sync::Arc::new(ydotool::YdotoolInjector::new(
                ydotool_socket.map(String::from),
            ))),
            other => {
                tracing::warn!("unknown injector preference: {other}");
                None
            }
        };
        let Some(injector) = candidate else { continue };
        match injector.probe() {
            Ok(()) => {
                tracing::info!("input injector active: {}", injector.name());
                return Ok(injector);
            }
            Err(e) => tracing::warn!("injector {} probe failed: {e:#}", injector.name()),
        }
    }
    anyhow::bail!("no working input injector (tried: {})", prefs.join(", "))
}

/// Normalize a spoken chord into ydotool/EIS key names: "Ctrl S" -> "ctrl+s".
pub fn normalize_chord(chord: &str) -> Vec<String> {
    chord
        .split(|c: char| c == '+' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|k| match k.to_lowercase().as_str() {
            "control" | "ctl" => "ctrl".to_string(),
            "super" | "win" | "windows" | "meta" => "meta".to_string(),
            "return" => "enter".to_string(),
            "escape" | "esc" => "escape".to_string(),
            "delete" | "del" => "delete".to_string(),
            other => other.to_string(),
        })
        .collect()
}
