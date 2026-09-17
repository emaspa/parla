//! Audio cues for record start/stop/error. KWin has no recording overlay
//! (wlroots-only in whisrs), so sound is the only state feedback (plan §1).

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;

#[derive(Clone, Copy, Debug)]
pub enum Cue {
    Start,
    Stop,
    Error,
}

impl Cue {
    fn file(self) -> &'static str {
        match self {
            Cue::Start => "cue-start.wav",
            Cue::Stop => "cue-stop.wav",
            Cue::Error => "cue-error.wav",
        }
    }
}

/// Play a cue without blocking; missing player or file is not fatal.
///
/// Callable from sync code on a runtime thread: the player runs as a task
/// that waits on the child, so no zombie is left behind.
pub fn play(cue: Cue) {
    let Some(path) = find_asset(cue.file()) else {
        tracing::trace!("cue asset {} not found; skipping", cue.file());
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("no tokio runtime; skipping cue {cue:?}");
        return;
    };
    handle.spawn(async move {
        for player in ["pw-play", "paplay"] {
            let spawned = Command::new(player)
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Ok(mut child) => {
                    match child.wait().await {
                        Ok(status) if !status.success() => {
                            tracing::debug!("{player} exited with {status} playing {cue:?}");
                        }
                        Ok(_) => {}
                        Err(e) => tracing::debug!("waiting on {player}: {e}"),
                    }
                    return;
                }
                Err(e) => tracing::trace!("{player} unavailable: {e}"),
            }
        }
        tracing::debug!("no cue player available (pw-play/paplay)");
    });
}

/// Look for assets next to the binary (installed layout), in the dev tree
/// (debug builds only), and under XDG_DATA_HOME.
fn find_asset(name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../share/parla/cues").join(name));
            candidates.push(dir.join("cues").join(name));
        }
    }
    #[cfg(debug_assertions)]
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets/cues")
            .join(name),
    );
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(data).join("parla/cues").join(name));
    }
    candidates.into_iter().find(|p| p.is_file())
}
