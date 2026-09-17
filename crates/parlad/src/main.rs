//! parlad: the parla daemon (plan P1).
//!
//! PipeWire capture (cpal) → energy-gated utterance validation → whisper.cpp
//! (CUDA) batch transcription → router: dictation keystrokes or fast-path
//! commands. PTT hotkeys via kglobalaccel press/release signals. Pauses
//! while the session is locked.

mod asr;
mod audio;
mod command;
mod config;
mod cues;
mod hotkeys;
mod instance;
mod judge;
mod lock;
mod policy;
mod router;
mod typesafe;
mod vad;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

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
    let instance = instance::InstanceLock::acquire()?;
    tracing::debug!("instance lock {}", instance.path().display());
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
    tokio::spawn(lock::watch_lock(conn.clone(), lock_tx));
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

    // One worker handles utterances in release order, so two quick
    // dictations can never be typed interleaved. The hotkey loop only
    // enqueues and stays responsive.
    let (job_tx, job_rx) = mpsc::channel::<Job>(JOB_QUEUE);
    let worker = tokio::spawn(worker(
        job_rx,
        Arc::clone(&cfg),
        Arc::clone(&asr),
        Arc::clone(&router),
        lock_rx.clone(),
    ));

    let mut capture: Option<Capture> = None;
    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;

    tracing::info!("parlad ready");
    let outcome = loop {
        let deadline = capture.as_ref().map(|c| c.deadline);
        tokio::select! {
            ev = hotkey_rx.recv() => {
                let Some(ev) = ev else {
                    break Err(anyhow::anyhow!(
                        "hotkey signal loop ended; exiting so the service manager restarts parlad"
                    ));
                };
                match ev {
                    HotkeyEvent::DictatePressed => {
                        start_capture(&mut capture, Mode::Dictate, &cfg, &lock_rx);
                    }
                    HotkeyEvent::CommandPressed => {
                        start_capture(&mut capture, Mode::Command, &cfg, &lock_rx);
                    }
                    HotkeyEvent::DictateReleased => {
                        finish_capture(&mut capture, Some(Mode::Dictate), &cfg, &lock_rx, &job_tx);
                    }
                    HotkeyEvent::CommandReleased => {
                        finish_capture(&mut capture, Some(Mode::Command), &cfg, &lock_rx, &job_tx);
                    }
                }
            }
            ready = capture_ready(&mut capture) => {
                match ready {
                    Ok(name) => {
                        let mode = capture.as_ref().map(|c| c.mode);
                        tracing::info!("{mode:?} capture started on {name}");
                    }
                    Err(e) => {
                        tracing::error!("capture start failed: {e:#}");
                        capture = None;
                        if cfg.router.cues {
                            cues::play(cues::Cue::Error);
                        }
                        notify("parla capture failed", &format!("{e:#}"));
                    }
                }
            }
            _ = hold_expired(deadline) => {
                tracing::warn!(
                    "hotkey held longer than {} ms without a release; finishing capture",
                    cfg.audio.max_hold_ms
                );
                finish_capture(&mut capture, None, &cfg, &lock_rx, &job_tx);
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGINT: shutting down");
                break Ok(());
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM: shutting down");
                break Ok(());
            }
        }
    };

    hotkeys.unregister().await;
    // A capture still running is abandoned: dropping it stops the thread.
    drop(capture);
    // Let a queued or in-flight utterance finish, but not for ever.
    drop(job_tx);
    match tokio::time::timeout(SHUTDOWN_GRACE, worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!("utterance worker panicked: {e}"),
        Err(_) => tracing::warn!(
            "utterance still in flight after {}s; abandoning it",
            SHUTDOWN_GRACE.as_secs()
        ),
    }
    drop(instance);
    outcome
}

/// Queue depth for finished captures waiting on ASR. Deeper than anyone can
/// hold-and-release in the time one utterance takes to transcribe.
const JOB_QUEUE: usize = 8;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// How long a stopped capture may take to hand over its samples.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// A running push-to-talk capture.
struct Capture {
    mode: Mode,
    session: CaptureSession,
    /// When the hold is treated as released even without a Released signal.
    deadline: Instant,
}

/// A finished capture, queued for transcription and routing.
struct Job {
    mode: Mode,
    samples: audio::Stopped,
}

/// Resolves when the active capture's device opens or fails; pends for ever
/// otherwise (or once it has resolved), so it is safe as a `select!` arm.
async fn capture_ready(capture: &mut Option<Capture>) -> anyhow::Result<String> {
    match capture {
        Some(c) => c.session.ready().await,
        None => std::future::pending().await,
    }
}

/// Resolves at the active capture's hold deadline; pends for ever otherwise.
async fn hold_expired(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

fn start_capture(
    capture: &mut Option<Capture>,
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
    // Room for the whole hold plus a little slack; the buffer is capped
    // there, so a runaway stream cannot eat memory.
    let max_samples = samples_for_ms(cfg.audio.max_hold_ms + 1_000);
    match CaptureSession::start(cfg.audio.device.clone(), max_samples) {
        Ok(session) => {
            tracing::debug!("{mode:?} capture starting");
            if cfg.router.cues {
                cues::play(cues::Cue::Start);
            }
            *capture = Some(Capture {
                mode,
                session,
                deadline: Instant::now() + Duration::from_millis(cfg.audio.max_hold_ms),
            });
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

/// Stop the active capture and queue it for processing. `released` is the
/// mode whose key was released, or None when the hold timer fired.
fn finish_capture(
    capture: &mut Option<Capture>,
    released: Option<Mode>,
    cfg: &DaemonConfig,
    lock_rx: &watch::Receiver<bool>,
    job_tx: &mpsc::Sender<Job>,
) {
    let Some(Capture { mode, session, .. }) = capture.take() else {
        tracing::debug!("release without active capture");
        return;
    };
    if let Some(released) = released {
        if released != mode {
            tracing::warn!("release {released:?} but captured {mode:?}; using captured mode");
        }
    }
    if cfg.router.cues {
        cues::play(cues::Cue::Stop);
    }
    let samples = session.stop();
    if *lock_rx.borrow() {
        // Locked between press and release: whatever was said goes nowhere.
        tracing::info!("session locked during {mode:?} capture; discarding utterance");
        if cfg.router.cues {
            cues::play(cues::Cue::Error);
        }
        return;
    }
    match job_tx.try_send(Job { mode, samples }) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::error!("{JOB_QUEUE} utterances already waiting on ASR; dropping this one");
            if cfg.router.cues {
                cues::play(cues::Cue::Error);
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::error!("utterance worker is gone; dropping {mode:?} utterance");
        }
    }
}

/// Process queued utterances one at a time, in the order they were released.
async fn worker(
    mut jobs: mpsc::Receiver<Job>,
    cfg: Arc<DaemonConfig>,
    asr: Arc<Asr>,
    router: Arc<Router>,
    lock_rx: watch::Receiver<bool>,
) {
    while let Some(Job { mode, samples }) = jobs.recv().await {
        let outcome = process(samples, &cfg.audio, &asr, &router, &lock_rx, mode).await;
        match outcome {
            Ok(Outcome::Done(msg)) => {
                tracing::info!("{mode:?} done: {msg}");
                if cfg.router.notify_results {
                    notify("parla", &msg);
                }
            }
            Ok(Outcome::Locked) => {
                tracing::info!("session locked; discarding transcribed {mode:?} utterance");
                if cfg.router.cues {
                    cues::play(cues::Cue::Error);
                }
            }
            Err(e) => {
                tracing::warn!("{mode:?} failed: {e:#}");
                if cfg.router.cues {
                    cues::play(cues::Cue::Error);
                }
                if cfg.router.notify_results {
                    notify("parla", &format!("{e:#}"));
                }
            }
        }
    }
}

enum Outcome {
    Done(String),
    /// The session locked before anything was injected or executed.
    Locked,
}

async fn process(
    samples: audio::Stopped,
    audio_cfg: &config::AudioConfig,
    asr: &Arc<Asr>,
    router: &Router,
    lock_rx: &watch::Receiver<bool>,
    mode: Mode,
) -> anyhow::Result<Outcome> {
    // The capture thread hands the samples over as soon as it closes the
    // stream; if PipeWire wedges that close, do not wedge the whole queue.
    let samples = tokio::time::timeout(STOP_GRACE, samples)
        .await
        .context("capture device did not stop in time")??;
    let trimmed = vad::validate(&samples, audio::SAMPLE_RATE, audio_cfg)?;
    let asr = Arc::clone(asr);
    let transcript = tokio::task::spawn_blocking(move || asr.transcribe(&trimmed))
        .await
        .context("ASR task panicked")??;
    anyhow::ensure!(!transcript.is_empty(), "heard nothing usable");
    // Last check before anything touches the desktop: transcription takes
    // long enough for the screen to have locked in the meantime.
    if *lock_rx.borrow() {
        return Ok(Outcome::Locked);
    }
    router.handle(mode, &transcript).await.map(Outcome::Done)
}

fn samples_for_ms(ms: u64) -> usize {
    (u64::from(audio::SAMPLE_RATE) * ms / 1000) as usize
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

    // The same executor, snapshot and policy the daemon would use, so what
    // this prints is what would have happened.
    let judge = Arc::new(judge::Judge::new(cfg.typesafe.clone())?);
    let executor = Arc::new(Executor::new(cfg.desktopd.clone()).await?);
    let router = Router::new(
        Arc::clone(&executor),
        Arc::new(grammar),
        Some(Arc::clone(&judge)),
    );
    let snapshot = router.snapshot().await?;
    let index = executor.desktop_index();
    println!(
        "state: {} apps indexed, {} windows open, desktop {}/{}, claude {}",
        index.entries().len(),
        snapshot.windows.len(),
        snapshot.current_desktop,
        snapshot.desktop_count,
        if snapshot.claude_running {
            "running"
        } else {
            "not running"
        }
    );

    match judge.judge_verdict(utterance, &snapshot.context(index)).await? {
        judge::Verdict::Act(resolved) => {
            println!("judged:    {:?}", resolved.intent);
            println!("confidence: {:.2}", resolved.confidence);
            let decision = router.policy().decide(&resolved.intent, &resolved.signals);
            let command = command::from_resolved(resolved);
            println!("command:   {command:?}");
            match decision {
                policy::Decision::Act => println!("would execute immediately"),
                policy::Decision::Confirm { reason } => {
                    println!("would ASK FOR CONFIRMATION before executing ({reason})")
                }
                policy::Decision::Refuse { reason } => println!("would refuse: {reason}"),
            }
        }
        judge::Verdict::Dictation => println!("judged:    dictation, not a command"),
        judge::Verdict::Unclear(r) => println!("judged:    unclear ({r})"),
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
