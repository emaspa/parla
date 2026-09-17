//! Audio cues for record start/stop/error. KWin has no recording overlay
//! (wlroots-only in whisrs), so sound is the primary state feedback next to
//! the tray icon (plan §1).

use std::process::{Command, Stdio};

#[derive(Clone, Copy)]
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
pub fn play(cue: Cue) {
    let Some(path) = find_asset(cue.file()) else {
        tracing::trace!("cue asset {} not found; skipping", cue.file());
        return;
    };
    for player in ["pw-play", "paplay"] {
        let spawned = Command::new(player)
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return;
        }
    }
    tracing::debug!("no cue player available (pw-play/paplay)");
}

/// Look for assets next to the binary (installed layout), in the dev tree,
/// and under XDG_DATA_HOME.
fn find_asset(name: &str) -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../share/parla/cues").join(name));
            candidates.push(dir.join("cues").join(name));
        }
    }
    candidates.push(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets/cues")
            .join(name),
    );
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        candidates.push(std::path::PathBuf::from(data).join("parla/cues").join(name));
    }
    candidates.into_iter().find(|p| p.is_file())
}
