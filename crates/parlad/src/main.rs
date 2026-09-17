//! parlad: the parla daemon (plan P1).
//!
//! PipeWire capture (cpal) → energy-gated utterance validation → whisper.cpp
//! (CUDA) batch transcription → router: dictation keystrokes or fast-path
//! commands. PTT hotkeys via kglobalaccel press/release signals. Pauses
//! while the session is locked.

mod asr;
mod audio;
mod config;
mod cues;
mod hotkeys;
mod judge;
mod lock;
mod router;
mod typesafe;
mod vad;

use std::sync::Arc;

use anyhow::Context as _;
use tokio::sync::watch;

use asr::Asr;
use audio::CaptureSession;
use config::DaemonConfig;
use desktopd::Executor;
use hotkeys::{HotkeyEvent, HotkeyManager};
use parla_grammar::Grammar;
use router::{Mode, Router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,parlad=debug,desktopd=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--print-default-config") => {
            print!("{}", DaemonConfig::default_toml()?);
            return Ok(());
        }
        Some("--check") => return check().await,
        Some("--judge") => {
            let utterance = args.get(1..).map(|r| r.join(" ")).unwrap_or_default();
            return judge_once(&utterance).await;
        }
        Some(other) => anyhow::bail!("unknown argument {other:?} (try --check or --judge)"),
        None => {}
    }
    run().await
}

async fn run() -> anyhow::Result<()> {
    let cfg = Arc::new(DaemonConfig::load()?);

    tracing::info!("starting executor (injector probe, desktop index)...");
    let executor = Arc::new(Executor::new(cfg.desktopd.clone()).await?);
    tracing::info!("input injector: {}", executor.injector_name());

    let grammar = Arc::new(load_grammar(&cfg)?);

    tracing::info!("loading whisper model...");
    let asr_cfg = cfg.asr.clone();
    let asr = Arc::new(
        tokio::task::spawn_blocking(move || Asr::load(&asr_cfg))
            .await
            .context("ASR load task panicked")??,
    );

    let (hotkeys, mut hotkey_rx) =
        HotkeyManager::register(&cfg.hotkeys.dictate, &cfg.hotkeys.command)
            .await
            .context("hotkey registration failed (is kglobalaccel reachable?)")?;
    tracing::info!(
        "PTT hotkeys: dictate={} command={}",
        cfg.hotkeys.dictate,
        cfg.hotkeys.command
    );

    // lock watcher: pause everything while the session is locked
    let conn = zbus::Connection::session().await?;
    let (lock_tx, lock_rx) = watch::channel(lock::is_locked(&conn).await);
    {
        let conn2 = conn.clone();
        tokio::spawn(async move {
            if let Err(e) = lock::watch_lock(conn2, lock_tx).await {
                tracing::error!("lock watcher died: {e:#}");
            }
        });
    }
    if *lock_rx.borrow() {
        tracing::info!("session is locked; waiting for unlock");
    }

    let judge = if cfg.typesafe.enabled {
        match judge::Judge::new(cfg.typesafe.clone()) {
            Ok(j) => {
                tracing::info!("judged path enabled (model {})", cfg.typesafe.model);
                Some(Arc::new(j))
            }
            Err(e) => {
                // Not fatal: the fast path is the point, judging is the net.
                tracing::warn!("judged path disabled: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let router = Arc::new(Router::new(
        Arc::clone(&executor),
        Arc::clone(&grammar),
        judge,
    ));
    let mut capture: Option<(Mode, CaptureSession)> = None;

    tracing::info!("parlad ready");
    loop {
        tokio::select! {
            ev = hotkey_rx.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    HotkeyEvent::DictatePressed => {
                        start_capture(&mut capture, Mode::Dictate, &cfg, &lock_rx);
                    }
                    HotkeyEvent::CommandPressed => {
                        start_capture(&mut capture, Mode::Command, &cfg, &lock_rx);
                    }
                    HotkeyEvent::DictateReleased => {
                        finish_capture(&mut capture, Mode::Dictate, &cfg, &asr, &router);
                    }
                    HotkeyEvent::CommandReleased => {
                        finish_capture(&mut capture, Mode::Command, &cfg, &asr, &router);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                hotkeys.unregister().await;
                break;
            }
        }
    }
    Ok(())
}

fn start_capture(
    capture: &mut Option<(Mode, CaptureSession)>,
    mode: Mode,
    cfg: &DaemonConfig,
    lock_rx: &watch::Receiver<bool>,
) {
    if capture.is_some() {
        tracing::warn!("hotkey pressed while already capturing; ignored");
        return;
    }
    if *lock_rx.borrow() {
        // locked session: input goes to the lock screen and voice must be
        // inert anyway (plan §5)
        tracing::info!("session locked; ignoring {mode:?} hotkey");
        notify("parla", "Session locked — voice paused");
        return;
    }
    match CaptureSession::start(cfg.audio.device.as_deref(), cfg.audio.sample_rate) {
        Ok(session) => {
            tracing::info!("{mode:?} capture started on {}", session.device_name());
            if cfg.router.cues {
                cues::play(cues::Cue::Start);
            }
            *capture = Some((mode, session));
        }
        Err(e) => {
            tracing::error!("capture start failed: {e:#}");
            if cfg.router.cues {
                cues::play(cues::Cue::Error);
            }
            notify("parla capture failed", &format!("{e:#}"));
        }
    }
}

fn finish_capture(
    capture: &mut Option<(Mode, CaptureSession)>,
    mode: Mode,
    cfg: &DaemonConfig,
    asr: &Arc<Asr>,
    router: &Arc<Router>,
) {
    let Some((captured_mode, session)) = capture.take() else {
        tracing::debug!("release without active capture");
        return;
    };
    if captured_mode != mode {
        tracing::warn!("release {mode:?} but captured {captured_mode:?}; using captured mode");
    }
    if cfg.router.cues {
        cues::play(cues::Cue::Stop);
    }
    let samples = session.stop();
    let rate = cfg.audio.sample_rate;
    let audio_cfg = cfg.audio.clone();
    let router_cfg = cfg.router.clone();
    let asr = Arc::clone(asr);
    let router = Arc::clone(router);
    tokio::spawn(async move {
        let outcome = process(samples, rate, &audio_cfg, asr, &router, captured_mode).await;
        match outcome {
            Ok(msg) => {
                tracing::info!("{captured_mode:?} done: {msg}");
                if router_cfg.notify_results {
                    notify("parla", &msg);
                }
            }
            Err(e) => {
                tracing::warn!("{captured_mode:?} failed: {e:#}");
                if router_cfg.cues {
                    cues::play(cues::Cue::Error);
                }
                if router_cfg.notify_results {
                    notify("parla", &format!("{e:#}"));
                }
            }
        }
    });
}

async fn process(
    samples: Vec<f32>,
    rate: u32,
    audio_cfg: &config::AudioConfig,
    asr: Arc<Asr>,
    router: &Router,
    mode: Mode,
) -> anyhow::Result<String> {
    let trimmed = vad::validate(&samples, rate, audio_cfg)?;
    let transcript = tokio::task::spawn_blocking(move || asr.transcribe(&trimmed))
        .await
        .context("ASR task panicked")??;
    anyhow::ensure!(!transcript.is_empty(), "heard nothing usable");
    router.handle(mode, &transcript).await
}

fn notify(summary: &str, body: &str) {
    let s = summary.to_string();
    let b = body.to_string();
    tokio::spawn(async move {
        if let Err(e) = desktopd::notify::notify(&s, &b).await {
            tracing::debug!("notification failed: {e}");
        }
    });
}

fn load_grammar(cfg: &DaemonConfig) -> anyhow::Result<Grammar> {
    match &cfg.router.grammar_file {
        Some(path) if path.exists() => {
            let text = std::fs::read_to_string(path)?;
            let g = Grammar::from_toml_str(&text)?;
            tracing::info!("loaded custom grammar rules from {}", path.display());
            Ok(g)
        }
        Some(path) => anyhow::bail!("grammar file {} not found", path.display()),
        None => Ok(Grammar::builtin()),
    }
}

/// `parlad --judge "<utterance>"`: run one utterance through the grammar and,
/// if it falls through, the judged path — printing what would happen without
/// executing it. Loads no ASR model and registers no hotkeys, so it is the way
/// to tune thresholds and phrasings against real state.
async fn judge_once(utterance: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !utterance.trim().is_empty(),
        "usage: parlad --judge \"<utterance>\""
    );
    let cfg = DaemonConfig::load()?;
    let grammar = load_grammar(&cfg)?;

    println!("utterance: {utterance:?}");
    if let Some(intent) = grammar.parse(utterance) {
        println!("fast path: {intent:?}");
        println!("(grammar matched; the judged path is not consulted)");
        return Ok(());
    }
    println!("fast path: no match");

    let judge = judge::Judge::new(cfg.typesafe.clone())?;
    let index = tokio::task::spawn_blocking(parla_grammar::DesktopIndex::from_xdg).await?;
    // Real window/desktop state when the desktop is reachable; empty otherwise,
    // so this stays usable over SSH.
    let windows = desktopd::windows::WindowCtl::new()
        .list()
        .await
        .unwrap_or_default();
    let desktop_count = desktopd::kwin::list_desktops()
        .await
        .map(|d| d.len() as u32)
        .unwrap_or(1);
    let current_desktop = desktopd::kwin::current_desktop().await.unwrap_or(1);
    println!(
        "state: {} apps indexed, {} windows open, desktop {current_desktop}/{desktop_count}",
        index.entries().len(),
        windows.len()
    );

    let ctx = judge::Context {
        windows: &windows,
        index: &index,
        current_desktop,
        desktop_count,
        claude_running: false,
    };
    match judge.judge(utterance, &ctx).await? {
        judge::Judgment::Act {
            intent,
            confidence,
            needs_confirmation,
        } => {
            println!("judged:    {intent:?}");
            println!("confidence: {confidence:.2}");
            println!(
                "would {}",
                if needs_confirmation {
                    "ASK FOR CONFIRMATION before executing"
                } else {
                    "execute immediately"
                }
            );
        }
        judge::Judgment::Dictation => println!("judged:    dictation, not a command"),
        judge::Judgment::Unclear(r) => println!("judged:    unclear ({r})"),
    }
    Ok(())
}

/// `parlad --check`: probe the environment without registering hotkeys.
async fn check() -> anyhow::Result<()> {
    let cfg = DaemonConfig::load()?;
    println!("config path:   {}", DaemonConfig::path().display());
    println!("model path:    {}", cfg.asr.model_path.display());
    println!(
        "model present: {}",
        if cfg.asr.model_path.exists() {
            "yes"
        } else {
            "NO — run scripts/fetch-model.sh"
        }
    );
    println!(
        "hotkeys:       dictate={} command={} (parsed: {:#x} {:#x})",
        cfg.hotkeys.dictate,
        cfg.hotkeys.command,
        hotkeys::parse_chord(&cfg.hotkeys.dictate)?,
        hotkeys::parse_chord(&cfg.hotkeys.command)?,
    );
    println!("input devices:");
    for d in audio::list_input_devices()? {
        println!("  - {d}");
    }
    println!(
        "probing injectors ({})...",
        cfg.desktopd.injectors.join(", ")
    );
    let executor = Executor::new(cfg.desktopd.clone()).await?;
    println!("active injector: {}", executor.injector_name());
    let windows = executor.list_windows().await?;
    println!("kdotool: {} windows visible", windows.len());
    let desktops = desktopd::kwin::list_desktops().await?;
    println!("virtual desktops: {}", desktops.len());
    match executor.resolve_app("konsole") {
        Ok(e) => println!("desktop index: resolved 'konsole' -> {}", e.id),
        Err(e) => println!("desktop index: FAILED ({e})"),
    }
    let conn = zbus::Connection::session().await?;
    println!("session locked: {}", lock::is_locked(&conn).await);
    println!("all checks passed");
    Ok(())
}
