//! App launching: kioclient first (handles .desktop Exec field codes,
//! startup notification, activities), gtk-launch for GTK ids, systemd-run
//! --user as the last resort.

use std::process::Stdio;

use parla_grammar::DesktopEntry;
use tokio::process::Command;

use crate::config::DesktopdConfig;

pub async fn launch_app(entry: &DesktopEntry, cfg: &DesktopdConfig) -> anyhow::Result<String> {
    let _ = cfg;
    let id = &entry.id;
    // kioclient exec applications:<id>
    let out = Command::new("kioclient")
        .args(["exec", &format!("applications:{id}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    match out {
        Ok(_) => return Ok(format!("launched {} via kioclient", entry.name)),
        Err(e) => tracing::debug!("kioclient spawn failed: {e}"),
    }
    // gtk-launch takes the id without .desktop
    let stem = id.trim_end_matches(".desktop");
    if which("gtk-launch").is_some() {
        let o = Command::new("gtk-launch")
            .arg(stem)
            .stderr(Stdio::piped())
            .output()
            .await?;
        if o.status.success() {
            return Ok(format!("launched {} via gtk-launch", entry.name));
        }
        tracing::debug!("gtk-launch failed: {}", String::from_utf8_lossy(&o.stderr));
    }
    // systemd-run --user with the parsed Exec line
    let exec = entry
        .exec
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no Exec= in {} and no launcher worked", id))?;
    let argv = parse_exec(exec);
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty Exec= in {id}"))?;
    let o = Command::new("systemd-run")
        .arg("--user")
        .arg(format!("--unit=parla-launch-{}", stem.replace('.', "-")))
        .arg(prog)
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .await?;
    if o.status.success() {
        Ok(format!("launched {} via systemd-run", entry.name))
    } else {
        anyhow::bail!(
            "all launchers failed for {id}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )
    }
}

/// Spawn the configured terminal, optionally running a command in it.
pub fn spawn_terminal(cfg: &DesktopdConfig, command: Option<Vec<String>>) -> anyhow::Result<()> {
    let mut c = Command::new(&cfg.terminal);
    c.args(&cfg.terminal_run_args);
    if let Some(argv) = command {
        c.args(argv);
    }
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Split a .desktop Exec= value, honoring quotes and dropping field codes (%u %f ...).
fn parse_exec(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quote = !in_quote,
            ' ' if !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '%' if !in_quote => {
                // field code: skip the next char
                chars.next();
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn which(prog: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let full = dir.join(prog);
            full.is_file().then_some(full)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::parse_exec;

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
    }
}
