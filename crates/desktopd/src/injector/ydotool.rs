//! Fallback injector: talks to the ydotoold uinput daemon over its socket.
//! Needs `ydotool.service` (user unit) running; no group membership needed
//! when the daemon runs as the user.

use std::time::Duration;

use super::{normalize_chord, TextInjector};
use crate::proc::Cmd;

/// Typing long dictation goes through ydotool's default 12 ms per key.
const TYPE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct YdotoolInjector {
    socket: Option<String>,
}

impl YdotoolInjector {
    pub fn new(socket: Option<String>) -> Self {
        Self { socket }
    }

    fn cmd(&self) -> Cmd {
        let c = Cmd::new("ydotool");
        match &self.socket {
            Some(sock) => c.env("YDOTOOL_SOCKET", sock),
            None => c,
        }
    }

    /// Where ydotoold puts its socket: `$YDOTOOL_SOCKET`, else
    /// `$XDG_RUNTIME_DIR/.ydotool_socket`, else `/run/user/<uid>/...`.
    fn socket_path(&self) -> String {
        if let Some(s) = &self.socket {
            return s.clone();
        }
        if let Ok(s) = std::env::var("YDOTOOL_SOCKET") {
            return s;
        }
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            // SAFETY: getuid has no preconditions and cannot fail
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
        format!("{runtime_dir}/.ydotool_socket")
    }
}

impl TextInjector for YdotoolInjector {
    fn name(&self) -> &'static str {
        "ydotool"
    }

    fn type_text(&self, text: &str) -> anyhow::Result<()> {
        // stdin avoids argv length limits and shell quoting entirely
        self.cmd()
            .args(["type", "--file=-"])
            .stdin(text)
            .timeout(TYPE_TIMEOUT)
            .run_blocking()?;
        Ok(())
    }

    fn key_chord(&self, chord: &str) -> anyhow::Result<()> {
        let keys = normalize_chord(chord);
        anyhow::ensure!(!keys.is_empty(), "empty key chord");
        let events = chord_events(&keys)?;
        self.cmd().arg("key").args(events).run_blocking()?;
        Ok(())
    }

    fn probe(&self) -> anyhow::Result<()> {
        self.cmd().arg("--help").run_blocking()?;
        // verify the daemon socket is reachable
        let socket = self.socket_path();
        anyhow::ensure!(
            std::path::Path::new(&socket).exists(),
            "ydotool socket not found at {socket} (is ydotoold running?)"
        );
        Ok(())
    }
}

/// Linux evdev keycode (linux/input-event-codes.h) for a normalized chord
/// key name.
pub fn keycode(key: &str) -> Option<u16> {
    let code = match key {
        "ctrl" => 29,
        "shift" => 42,
        "alt" => 56,
        "meta" => 125,
        "altgr" => 100,
        "enter" => 28,
        "escape" => 1,
        "backspace" => 14,
        "tab" => 15,
        "space" => 57,
        "delete" => 111,
        "insert" => 110,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "up" => 103,
        "left" => 105,
        "right" => 106,
        "down" => 108,
        "minus" | "-" => 12,
        "equal" | "=" => 13,
        "comma" | "," => 51,
        "period" | "." => 52,
        "slash" | "/" => 53,
        "semicolon" | ";" => 39,
        "apostrophe" | "'" => 40,
        "grave" | "`" => 41,
        "leftbrace" | "[" => 26,
        "rightbrace" | "]" => 27,
        "backslash" | "\\" => 43,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        _ => {
            let mut chars = key.chars();
            let (c, rest) = (chars.next()?, chars.next());
            if rest.is_some() {
                return None;
            }
            match c.to_ascii_lowercase() {
                // US layout row order, as uinput sees it
                'q' => 16, 'w' => 17, 'e' => 18, 'r' => 19, 't' => 20, 'y' => 21,
                'u' => 22, 'i' => 23, 'o' => 24, 'p' => 25,
                'a' => 30, 's' => 31, 'd' => 32, 'f' => 33, 'g' => 34, 'h' => 35,
                'j' => 36, 'k' => 37, 'l' => 38,
                'z' => 44, 'x' => 45, 'c' => 46, 'v' => 47, 'b' => 48, 'n' => 49,
                'm' => 50,
                '1' => 2, '2' => 3, '3' => 4, '4' => 5, '5' => 6, '6' => 7, '7' => 8,
                '8' => 9, '9' => 10, '0' => 11,
                _ => return None,
            }
        }
    };
    Some(code)
}

/// `ydotool key` arguments for a chord: every key pressed in order, then
/// released in reverse.
pub fn chord_events(keys: &[String]) -> anyhow::Result<Vec<String>> {
    let codes: Vec<u16> = keys
        .iter()
        .map(|k| keycode(k).ok_or_else(|| anyhow::anyhow!("no evdev keycode for key {k:?}")))
        .collect::<anyhow::Result<_>>()?;
    let mut ev = Vec::with_capacity(codes.len() * 2);
    ev.extend(codes.iter().map(|c| format!("{c}:1")));
    ev.extend(codes.iter().rev().map(|c| format!("{c}:0")));
    Ok(ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_shift_s_maps_to_press_then_reverse_release() {
        let keys = normalize_chord("ctrl+shift+s");
        assert_eq!(
            chord_events(&keys).unwrap(),
            vec!["29:1", "42:1", "31:1", "31:0", "42:0", "29:0"]
        );
    }

    #[test]
    fn spoken_names_and_unknowns() {
        assert_eq!(chord_events(&normalize_chord("Control Return")).unwrap(), vec!["29:1", "28:1", "28:0", "29:0"]);
        assert_eq!(keycode("f12"), Some(88));
        assert_eq!(keycode("left"), Some(105));
        assert_eq!(keycode("7"), Some(8));
        assert_eq!(keycode("S"), Some(31));
        assert_eq!(keycode("hyper"), None);
        assert!(chord_events(&normalize_chord("ctrl+hyper")).is_err());
    }
}
