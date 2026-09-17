//! parla-probe: live test harness for the desktopd tool surface.
//! Also serves as the P0 environment probe (EIS check, window ops, apps).
//!
//! Usage:
//!   parla-probe injector            # select + report active injector
//!   parla-probe windows             # list windows
//!   parla-probe snapshot            # everything the judged path sees, as JSON
//!   parla-probe resolve QUERY       # which window QUERY would target (no action)
//!   parla-probe desktops            # list virtual desktops, mark current
//!   parla-probe apps QUERY          # resolve QUERY against the .desktop index
//!   parla-probe type TEXT           # type into the FOCUSED window (careful)
//!   parla-probe key CHORD           # send a key chord ("ctrl+s")
//!   parla-probe shortcut COMP ACT   # invoke a kglobalaccel shortcut
//!   parla-probe krunner QUERY       # open krunner prefilled

use anyhow::Context as _;
use desktopd::config::DesktopdConfig;
use desktopd::Executor;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "desktopd=debug,warn".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("injector");

    match cmd {
        "injector" => {
            let exec = Executor::new(DesktopdConfig::default()).await?;
            println!("active injector: {}", exec.injector_name());
        }
        "windows" => {
            let exec = Executor::new(DesktopdConfig::default()).await?;
            for w in exec.list_windows().await? {
                println!("{}  [{}]  {}", w.id, w.class, w.title);
            }
        }
        "snapshot" => {
            let exec = Executor::new(DesktopdConfig::default()).await?;
            println!("{}", serde_json::to_string_pretty(&exec.snapshot().await)?);
        }
        "resolve" => {
            let query = args.get(1).context("usage: resolve QUERY")?;
            let exec = Executor::new(DesktopdConfig::default()).await?;
            match exec.resolve_window(query).await {
                Ok(w) => println!("{} [{}] {}", w.id, w.class, w.title),
                Err(e) => println!("{query} -> {e:#}"),
            }
        }
        "desktops" => {
            let desktops = desktopd::kwin::list_desktops().await?;
            for (pos, id, name) in &desktops {
                println!("{pos}: {name} ({id})");
            }
        }
        "apps" => {
            let query = args.get(1).context("usage: apps QUERY")?;
            let exec = Executor::new(DesktopdConfig {
                focus_if_running: false,
                ..Default::default()
            })
            .await?;
            match exec.resolve_app(query) {
                Ok(e) => println!("{} -> {} ({})", query, e.name, e.id),
                Err(err) => println!("{query} -> no match ({err})"),
            }
        }
        "type" => {
            let text = args.get(1).context("usage: type TEXT")?;
            let exec = Executor::new(DesktopdConfig::default()).await?;
            println!("{}", exec.type_text(text).await?);
        }
        "key" => {
            let chord = args.get(1).context("usage: key CHORD")?;
            let exec = Executor::new(DesktopdConfig::default()).await?;
            println!("{}", exec.key(chord).await?);
        }
        "shortcut" => {
            let component = args.get(1).context("usage: shortcut COMPONENT ACTION")?;
            let action = args.get(2).context("usage: shortcut COMPONENT ACTION")?;
            let exec = Executor::new(DesktopdConfig::default()).await?;
            println!("{}", exec.run_shortcut(component, action).await?);
        }
        "krunner" => {
            let query = args.get(1).context("usage: krunner QUERY")?;
            desktopd::kwin::krunner_query(query).await?;
            println!("krunner opened with {query:?}");
        }
        "screenshot" => {
            let out = args.get(1).context("usage: screenshot OUT.ppm")?;
            let shot = desktopd::screenshot::capture_active_window().await?;
            println!(
                "{}x{} stride={} format={}",
                shot.width, shot.height, shot.stride, shot.format
            );
            shot.write_ppm(std::path::Path::new(out))?;
            println!("wrote {out}");
        }
        other => anyhow::bail!("unknown command {other:?}"),
    }
    Ok(())
}
