//! App launching: kioclient first (handles .desktop Exec field codes,
//! startup notification, activities), gtk-launch for GTK ids, systemd-run
//! --user as the last resort.

use std::time::Duration;

use crate::config::DesktopdConfig;
use crate::desktop::DesktopEntry;
use crate::proc::{which, Cmd, ProcError};

/// kioclient exits once the app is spawned; gtk-launch likewise. Past this,
/// something is wrong with the launcher, not the app.
const LAUNCHER_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn launch_app(entry: &DesktopEntry, cfg: &DesktopdConfig) -> anyhow::Result<String> {
    let id = &entry.id;
    let mut failures: Vec<String> = Vec::new();

    // kioclient exec applications:<id>
    match Cmd::new("kioclient")
        .args(["exec", &format!("applications:{id}")])
        .timeout(LAUNCHER_TIMEOUT)
        .output()
        .await
    {
        Ok(o) if o.success() => return Ok(format!("launched {} via kioclient", entry.name)),
        Ok(o) => {
            let err = o.stderr.trim().to_string();
            tracing::warn!("kioclient exec {id} failed ({}): {err}", o.status);
            failures.push(format!("kioclient: {err}"));
        }
        // still alive after the timeout: KIO usually exits after the spawn,
        // but a slow startup-notification handshake can hold it. The app is
        // most likely up; launching again would open it twice.
        Err(ProcError::Timeout { .. }) => {
            tracing::warn!("kioclient exec {id} still running after {LAUNCHER_TIMEOUT:?}; assuming launched");
            return Ok(format!("launched {} via kioclient (unconfirmed)", entry.name));
        }
        Err(e) => {
            tracing::debug!("{e}");
            failures.push(e.to_string());
        }
    }

    // gtk-launch takes the id without .desktop
    let stem = id.trim_end_matches(".desktop");
    if which("gtk-launch").is_some() {
        match Cmd::new("gtk-launch")
            .arg(stem)
            .timeout(LAUNCHER_TIMEOUT)
            .run()
            .await
        {
            Ok(_) => return Ok(format!("launched {} via gtk-launch", entry.name)),
            Err(e) => {
                tracing::debug!("{e}");
                failures.push(e.to_string());
            }
        }
    }

    // systemd-run --user with the parsed Exec line
    let exec = entry.exec.as_ref().ok_or_else(|| {
        anyhow::anyhow!("no Exec= in {id} and no launcher worked: {}", failures.join("; "))
    })?;
    let mut argv = parse_exec(exec);
    anyhow::ensure!(!argv.is_empty(), "empty Exec= in {id}");
    if entry.terminal {
        argv = wrap_in_terminal(cfg, argv);
    }
    let unit = format!(
        "parla-launch-{}-{}",
        stem.replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
        std::process::id() ^ (unix_time_ms() as u32)
    );
    match Cmd::new("systemd-run")
        .args(["--user", "--collect", &format!("--unit={unit}")])
        .args(argv)
        .timeout(LAUNCHER_TIMEOUT)
        .run()
        .await
    {
        Ok(_) => Ok(format!("launched {} via systemd-run", entry.name)),
        Err(e) => {
            failures.push(e.to_string());
            anyhow::bail!("all launchers failed for {id}: {}", failures.join("; "))
        }
    }
}

/// `Terminal=true`: the program wants a tty. Run it inside the configured
/// terminal, or xterm when that one is not installed.
fn wrap_in_terminal(cfg: &DesktopdConfig, argv: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len() + 2);
    if which(&cfg.terminal).is_some() {
        out.push(cfg.terminal.clone());
        out.extend(cfg.terminal_run_args.iter().cloned());
    } else {
        tracing::warn!("terminal {:?} not on PATH; using xterm", cfg.terminal);
        out.push("xterm".into());
        out.push("-e".into());
    }
    out.extend(argv);
    out
}

fn unix_time_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Spawn the configured terminal, optionally running a command in it.
pub fn spawn_terminal(cfg: &DesktopdConfig, command: Option<Vec<String>>) -> anyhow::Result<()> {
    let mut c = Cmd::new(&cfg.terminal).args(cfg.terminal_run_args.iter().cloned());
    if let Some(argv) = command {
        c = c.args(argv);
    }
    c.spawn_detached()?;
    Ok(())
}

/// Split a .desktop Exec= value per the Desktop Entry spec: arguments are
/// separated by spaces; double quotes group; inside quotes a backslash
/// escapes `"`, `` ` ``, `$` and `\`; `%%` is a literal percent and every
/// other `%X` field code is dropped (we have nothing to open).
pub fn parse_exec(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false; // this argument had quotes: keep it even if empty
    let mut in_quote = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quote = !in_quote;
                quoted = true;
            }
            '\\' if in_quote => match chars.next() {
                Some(e @ ('"' | '`' | '$' | '\\')) => cur.push(e),
                Some(other) => {
                    cur.push('\\');
                    cur.push(other);
                }
                None => cur.push('\\'),
            },
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() || quoted {
                    out.push(std::mem::take(&mut cur));
                }
                quoted = false;
            }
            // %% is a percent; any other field code (%u, %F, %i, ...) has
            // nothing to substitute and vanishes
            '%' => {
                if chars.next() == Some('%') {
                    cur.push('%');
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() || quoted {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_parsing() {
        assert_eq!(
            parse_exec("firefox --new-window %u"),
            vec!["firefox", "--new-window"]
        );
        assert_eq!(
            parse_exec(r#"/usr/lib/x86_64-linux-gnu/cef/cef --type=thing "a b" %f"#),
            vec![
                "/usr/lib/x86_64-linux-gnu/cef/cef",
                "--type=thing",
                "a b"
            ]
        );
        // %% is a literal percent, inside and outside quotes
        assert_eq!(
            parse_exec(r#"prog --pct=100%% "50%% off" %F"#),
            vec!["prog", "--pct=100%", "50% off"]
        );
        // backslash escapes inside double quotes
        assert_eq!(
            parse_exec(r#"sh -c "echo \"hi\" \$HOME \\ \`x\`""#),
            vec!["sh", "-c", r#"echo "hi" $HOME \ `x`"#]
        );
        // a backslash before anything else stays a backslash
        assert_eq!(parse_exec(r#""C:\path""#), vec![r"C:\path"]);
        // a quoted empty argument survives; a bare field code does not
        assert_eq!(parse_exec(r#"prog "" %u"#), vec!["prog", ""]);
        // tabs separate too; multiple spaces collapse
        assert_eq!(parse_exec("a  b\tc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn terminal_wrapping_uses_config() {
        let cfg = DesktopdConfig {
            terminal: "sh".into(), // on every PATH
            terminal_run_args: vec!["-c".into()],
            ..Default::default()
        };
        assert_eq!(
            wrap_in_terminal(&cfg, vec!["htop".into()]),
            vec!["sh", "-c", "htop"]
        );
        let cfg = DesktopdConfig {
            terminal: "parla-no-such-terminal".into(),
            ..Default::default()
        };
        assert_eq!(
            wrap_in_terminal(&cfg, vec!["htop".into()]),
            vec!["xterm", "-e", "htop"]
        );
    }
}
