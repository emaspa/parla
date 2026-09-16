use std::io::Write;
use std::process::{Command, Stdio};

use super::{normalize_chord, TextInjector};

/// Fallback injector: talks to the ydotoold uinput daemon over its socket.
/// Needs `ydotool.service` (user unit) running; no group membership needed
/// when the daemon runs as the user.
pub struct YdotoolInjector {
    socket: Option<String>,
}

impl YdotoolInjector {
    pub fn new(socket: Option<String>) -> Self {
        Self { socket }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new("ydotool");
        if let Some(sock) = &self.socket {
            c.env("YDOTOOL_SOCKET", sock);
        }
        c
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> anyhow::Result<()> {
        let mut child = match stdin {
            Some(_) => self
                .cmd()
                .args(args)
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
            None => self
                .cmd()
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?,
        };
        if let (Some(text), Some(mut sin)) = (stdin, child.stdin.take()) {
            sin.write_all(text.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            anyhow::bail!(
                "ydotool {} failed: {}",
                args.first().copied().unwrap_or("?"),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

impl TextInjector for YdotoolInjector {
    fn name(&self) -> &'static str {
        "ydotool"
    }

    fn type_text(&self, text: &str) -> anyhow::Result<()> {
        // stdin avoids argv length limits and shell quoting entirely
        self.run(&["type", "--file=-"], Some(text))
    }

    fn key_chord(&self, chord: &str) -> anyhow::Result<()> {
        let keys = normalize_chord(chord);
        anyhow::ensure!(!keys.is_empty(), "empty key chord");
        self.run(&["key", &keys.join("+")], None)
    }

    fn probe(&self) -> anyhow::Result<()> {
        let out = self.cmd().arg("--help").output();
        match out {
            Ok(o) if o.status.success() => {
                // verify the daemon socket is reachable
                let socket = self.socket.clone().unwrap_or_else(|| {
                    format!(
                        "{}/.ydotool_socket",
                        std::env::var("XDG_RUNTIME_DIR")
                            .unwrap_or_else(|_| "/run/user/1000".to_string())
                    )
                });
                anyhow::ensure!(
                    std::path::Path::new(&socket).exists(),
                    "ydotool socket not found at {socket} (is ydotoold running?)"
                );
                Ok(())
            }
            Ok(o) => anyhow::bail!(
                "ydotool --help failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => anyhow::bail!("ydotool not executable: {e}"),
        }
    }
}
