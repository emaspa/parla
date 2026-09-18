//! parlad: the parla daemon (plan P1).
//!
//! PipeWire capture (cpal) → speech gate (Silero VAD, or the RMS energy gate
//! without its model) → whisper.cpp (CUDA) batch transcription → router:
//! dictation keystrokes or fast-path commands. PTT hotkeys via kglobalaccel
//! press/release signals. Pauses while the session is locked.

mod asr;
mod audio;
mod calibrate;
mod command;
mod config;
mod confirm;
mod cues;
mod dbus;
mod flow;
mod hotkeys;
mod instance;
mod judge;
mod local;
mod lock;
mod openai;
mod oracle;
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
use dbus::{Bus, Control, Notice, State};
use desktopd::{A11y, Executor, FocusedText, Window};
use flow::{Flow, TextContext};
use hotkeys::{HotkeyEvent, HotkeyManager};
use local::LocalModel;
use parla_flow::Record;
use parla_grammar::Grammar;
use router::{Handled, Mode, Router};
use vad::Gate;

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
        Some("--calibrate") => {
            let path = args.get(1).map_or("corpus/judge.toml", String::as_str);
            return calibrate::run(std::path::Path::new(path)).await;
        }
        Some("--flow") => {
            let text = args.get(1).cloned().unwrap_or_default();
            let class = args.get(2).cloned().unwrap_or_default();
            return flow_once(&text, None, &class).await;
        }
        Some("--edit") => {
            let text = args.get(1).cloned().unwrap_or_default();
            let instruction = args.get(2).cloned().unwrap_or_default();
            let class = args.get(3).cloned().unwrap_or_default();
            return flow_once(&text, Some(&instruction), &class).await;
        }
        Some(other) => {
            anyhow::bail!("unknown argument {other:?} (try --check, --judge, --calibrate, --flow or --edit)")
        }
        None => {}
    }
    run().await
}

/// Load the shared GGUF when any backend asks for it. A failure disables
/// the paths that wanted it rather than the daemon: the fast path and raw
/// dictation still work.
async fn load_local(cfg: &DaemonConfig) -> Option<Arc<LocalModel>> {
    let judge_wants = cfg.judge.enabled && cfg.judge.backend == config::Backend::Local;
    let flow_wants = cfg.flow.cleanup && cfg.flow.backend == config::FlowBackend::Local;
    if !judge_wants && !flow_wants {
        return None;
    }
    tracing::info!("loading local model...");
    let local_cfg = cfg.local.clone();
    match tokio::task::spawn_blocking(move || LocalModel::load(&local_cfg)).await {
        Ok(Ok(m)) => Some(Arc::new(m)),
        Ok(Err(e)) => {
            tracing::warn!("local model unavailable: {e:#}");
            None
        }
        Err(e) => {
            tracing::warn!("local model load task panicked: {e}");
            None
        }
    }
}

fn load_judge(cfg: &DaemonConfig, local: Option<Arc<LocalModel>>) -> Option<Arc<judge::Judge>> {
    if !cfg.judge.enabled {
        return None;
    }
    match judge::Judge::new(&cfg.judge, local) {
        Ok(j) => {
            tracing::info!("judged path enabled ({})", j.describe());
            Some(Arc::new(j))
        }
        Err(e) => {
            // Not fatal: the fast path is the point, judging is the net.
            tracing::warn!("judged path disabled: {e:#}");
            None
        }
    }
}

/// The dictation flow, with cleanup off if its backend is unavailable. A
/// dictionary or snippet file that does not parse is still an error.
fn load_flow(cfg: &DaemonConfig, local: Option<Arc<LocalModel>>) -> anyhow::Result<Arc<Flow>> {
    let prompt = cfg.asr.initial_prompt.clone();
    let flow = match Flow::load(&cfg.flow, prompt.clone(), local) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("dictation cleanup disabled: {e:#}");
            let mut off = cfg.flow.clone();
            off.cleanup = false;
            Flow::load(&off, prompt, None)?
        }
    };
    tracing::info!("dictation cleanup: {}", flow.describe());
    Ok(Arc::new(flow))
}

async fn run() -> anyhow::Result<()> {
    let instance = instance::InstanceLock::acquire()?;
    tracing::debug!("instance lock {}", instance.path().display());
    let cfg = Arc::new(DaemonConfig::load()?);

    tracing::info!("starting executor (injector probe, desktop index)...");
    let executor = Executor::new(cfg.desktopd.clone()).await?;
    tracing::info!("input injector: {}", executor.injector_name());

    // The a11y flag is set here and cleared on the way out, whatever the
    // way out is, so a failed start does not leave it on.
    let a11y = connect_a11y(&cfg).await;
    let executor = Arc::new(executor.with_a11y(a11y.clone()));
    let outcome = serve(instance, cfg, executor).await;
    if let Some(a11y) = a11y {
        a11y.shutdown().await;
    }
    outcome
}

/// The accessibility bus, when the config wants it. Not fatal: without it
/// cleanup does not see the text around the cursor and edits are blind.
async fn connect_a11y(cfg: &DaemonConfig) -> Option<Arc<A11y>> {
    if !cfg.desktopd.a11y {
        tracing::info!("a11y off in the config; cleanup will not see the text around the cursor");
        return None;
    }
    match A11y::connect(true).await {
        Ok(a) => Some(Arc::new(a)),
        Err(e) => {
            tracing::warn!("a11y unavailable ({e:#}); cleanup will not see the text around the cursor");
            None
        }
    }
}

async fn serve(
    instance: instance::InstanceLock,
    cfg: Arc<DaemonConfig>,
    executor: Arc<Executor>,
) -> anyhow::Result<()> {
    let grammar = Arc::new(load_grammar(&cfg)?);

    tracing::info!("loading whisper model...");
    let asr_cfg = cfg.asr.clone();
    let asr = Arc::new(
        tokio::task::spawn_blocking(move || Asr::load(&asr_cfg))
            .await
            .context("ASR load task panicked")??,
    );

    let gate = Arc::new(Gate::load(&cfg.asr, &cfg.audio));

    let local = load_local(&cfg).await;
    let judge = load_judge(&cfg, local.clone());
    let flow = load_flow(&cfg, local)?;

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
    let conn = desktopd::bus::session().await?;
    let (lock_tx, lock_rx) = watch::channel(lock::is_locked(&conn).await);
    tokio::spawn(lock::watch_lock(conn.clone(), lock_tx));
    if *lock_rx.borrow() {
        tracing::info!("session is locked; waiting for unlock");
    }

    // The UI's view of the daemon. Without a bus there is no UI, and
    // nothing else changes.
    let info = dbus::Info {
        version: env!("CARGO_PKG_VERSION").into(),
        judge: judge.as_ref().map(|j| j.describe()).unwrap_or_default(),
        cleanup: if flow.cleanup_enabled() {
            flow.describe()
        } else {
            String::new()
        },
        dictate_hotkey: cfg.hotkeys.dictate.clone(),
        command_hotkey: cfg.hotkeys.command.clone(),
    };
    let (bus, mut control_rx) = match dbus::serve(&conn, Arc::clone(&flow), info).await {
        Ok((bus, rx)) => {
            tracing::info!("on the session bus as {}", dbus::NAME);
            (Some(bus), Some(rx))
        }
        Err(e) => {
            tracing::warn!("not on the session bus ({e:#}); the UI will not see this daemon");
            (None, None)
        }
    };
    if let Some(bus) = &bus {
        tokio::spawn(publish_lock(lock_rx.clone(), bus.clone()));
    }

    let router = Arc::new(Router::new(
        Arc::clone(&executor),
        Arc::clone(&grammar),
        judge,
        Arc::clone(&flow),
        Duration::from_millis(cfg.router.confirm_window_ms),
        bus.clone(),
    ));

    // One worker handles utterances in release order, so two quick
    // dictations can never be typed interleaved. The hotkey loop only
    // enqueues and stays responsive.
    let (job_tx, job_rx) = mpsc::channel::<Job>(JOB_QUEUE);
    let worker = tokio::spawn(worker(
        job_rx,
        Arc::clone(&cfg),
        Arc::clone(&gate),
        Arc::clone(&asr),
        Arc::clone(&router),
        lock_rx.clone(),
        bus.clone(),
    ));

    let mut capture: Option<Capture> = None;
    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let ctx = LoopCtx {
        cfg: &cfg,
        executor: &executor,
        lock_rx: &lock_rx,
        bus: bus.as_ref(),
        job_tx: &job_tx,
    };

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
                let enabled = bus.as_ref().is_none_or(Bus::enabled);
                match ev {
                    HotkeyEvent::DictatePressed | HotkeyEvent::CommandPressed if !enabled => {
                        tracing::info!("disabled from the UI; ignoring {ev:?}");
                    }
                    HotkeyEvent::DictatePressed => {
                        if let Err(e) = start_capture(&mut capture, Mode::Dictate, &ctx) {
                            tracing::debug!("{e:#}");
                        }
                    }
                    HotkeyEvent::CommandPressed => {
                        if let Err(e) = start_capture(&mut capture, Mode::Command, &ctx) {
                            tracing::debug!("{e:#}");
                        }
                    }
                    HotkeyEvent::DictateReleased => {
                        finish_capture(&mut capture, Some(Mode::Dictate), &ctx);
                    }
                    HotkeyEvent::CommandReleased => {
                        finish_capture(&mut capture, Some(Mode::Command), &ctx);
                    }
                }
            }
            control = next_control(&mut control_rx) => {
                match control {
                    Control::Start(mode, reply) => {
                        let _ = reply.send(start_capture(&mut capture, mode, &ctx));
                    }
                    Control::Stop(reply) => {
                        let _ = reply.send(if capture.is_some() {
                            finish_capture(&mut capture, None, &ctx);
                            Ok(())
                        } else {
                            Err(anyhow::anyhow!("not recording"))
                        });
                    }
                    Control::Cancel(reply) => {
                        let _ = reply.send(match capture.take() {
                            Some(c) => {
                                tracing::info!("{:?} capture cancelled from the UI", c.mode);
                                drop(c);
                                if let Some(bus) = &bus {
                                    bus.finish("cancelled", false);
                                }
                                Ok(())
                            }
                            None => Err(anyhow::anyhow!("not recording")),
                        });
                    }
                    Control::Enabled(enabled) => {
                        tracing::info!("{} from the UI", if enabled { "enabled" } else { "disabled" });
                        if let Some(bus) = &bus {
                            if capture.is_none() && matches!(bus.state(), State::Idle | State::Paused) {
                                let paused = !enabled || *lock_rx.borrow();
                                bus.set_state(if paused { State::Paused } else { State::Idle }, None);
                            }
                        }
                    }
                }
            }
            ready = capture_ready(&mut capture) => {
                match ready {
                    Ok(name) => {
                        let mode = capture.as_ref().map(|c| c.mode);
                        tracing::info!("{mode:?} capture started on {name}");
                        // The cue is the "speak now" signal, so it waits
                        // for the device to be open and recording.
                        if cfg.router.cues {
                            cues::play(cues::Cue::Start);
                        }
                    }
                    Err(e) => {
                        tracing::error!("capture start failed: {e:#}");
                        capture = None;
                        ctx.fail(&format!("capture failed: {e:#}"));
                    }
                }
            }
            _ = hold_expired(deadline) => {
                tracing::warn!(
                    "hotkey held longer than {} ms without a release; finishing capture",
                    cfg.audio.max_hold_ms
                );
                finish_capture(&mut capture, None, &ctx);
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

/// What the main loop's capture handling needs from the daemon.
struct LoopCtx<'a> {
    cfg: &'a DaemonConfig,
    executor: &'a Arc<Executor>,
    lock_rx: &'a watch::Receiver<bool>,
    bus: Option<&'a Bus>,
    job_tx: &'a mpsc::Sender<Job>,
}

impl LoopCtx<'_> {
    /// An error before anything was transcribed: cue, notify, tell the UI.
    fn fail(&self, message: &str) {
        if self.cfg.router.cues {
            cues::play(cues::Cue::Error);
        }
        notify("parla", message);
        if let Some(bus) = self.bus {
            bus.finish(message, true);
            bus.notice(Notice::Error(message.to_string()));
        }
    }
}

/// A running push-to-talk capture.
struct Capture {
    mode: Mode,
    session: CaptureSession,
    /// When the hold is treated as released even without a Released signal.
    deadline: Instant,
    /// What had focus when the key went down, looked up in the background
    /// so the capture starts without waiting on kdotool or the a11y bus.
    focus: tokio::task::JoinHandle<Focus>,
}

/// Where the text will land: the focused window, and the focused text
/// field when the a11y bus can read one.
#[derive(Default)]
struct Focus {
    window: Option<Window>,
    text: Option<FocusedText>,
}

/// A finished capture, queued for transcription and routing.
struct Job {
    mode: Mode,
    samples: audio::Stopped,
    focus: tokio::task::JoinHandle<Focus>,
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

/// The next request from the bus; pends for ever without a bus, or once
/// the bus side is gone, so it is safe as a `select!` arm.
async fn next_control(rx: &mut Option<mpsc::Receiver<Control>>) -> Control {
    match rx {
        Some(rx) => match rx.recv().await {
            Some(c) => c,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Mirror the session lock onto the bus state while nothing is in flight.
async fn publish_lock(mut lock_rx: watch::Receiver<bool>, bus: Bus) {
    while lock_rx.changed().await.is_ok() {
        let locked = *lock_rx.borrow();
        match (locked, bus.state()) {
            (true, State::Idle) => bus.set_state(State::Paused, None),
            (false, State::Paused) if bus.enabled() => bus.set_state(State::Idle, None),
            _ => {}
        }
    }
}

fn start_capture(capture: &mut Option<Capture>, mode: Mode, ctx: &LoopCtx<'_>) -> anyhow::Result<()> {
    if capture.is_some() {
        tracing::warn!("capture requested while already capturing; ignored");
        anyhow::bail!("already recording");
    }
    if *ctx.lock_rx.borrow() {
        // locked session: input goes to the lock screen and voice must be
        // inert anyway (plan §5)
        tracing::info!("session locked; ignoring {mode:?} request");
        notify("parla", "Session locked — voice paused");
        anyhow::bail!("the session is locked");
    }
    let cfg = ctx.cfg;
    // Room for the whole hold plus a little slack; the buffer is capped
    // there, so a runaway stream cannot eat memory.
    let max_samples = samples_for_ms(cfg.audio.max_hold_ms + 1_000);
    let level = ctx.bus.map(Bus::level);
    match CaptureSession::start(cfg.audio.device.clone(), max_samples, level) {
        Ok(session) => {
            tracing::debug!("{mode:?} capture starting");
            let executor = Arc::clone(ctx.executor);
            let want_text = cfg.flow.context && mode == Mode::Dictate;
            let focus = tokio::spawn(async move {
                let text = async {
                    if want_text {
                        executor.focused_text().await
                    } else {
                        None
                    }
                };
                let (window, text) = tokio::join!(executor.active_window(), text);
                Focus {
                    window: window.ok().flatten(),
                    text,
                }
            });
            *capture = Some(Capture {
                mode,
                session,
                deadline: Instant::now() + Duration::from_millis(cfg.audio.max_hold_ms),
                focus,
            });
            if let Some(bus) = ctx.bus {
                bus.set_state(State::Recording, Some(mode));
            }
            Ok(())
        }
        Err(e) => {
            tracing::error!("capture start failed: {e:#}");
            ctx.fail(&format!("capture failed: {e:#}"));
            Err(e)
        }
    }
}

/// Stop the active capture and queue it for processing. `released` is the
/// mode whose key was released, or None when the hold timer fired or the
/// UI asked.
fn finish_capture(capture: &mut Option<Capture>, released: Option<Mode>, ctx: &LoopCtx<'_>) {
    let Some(Capture {
        mode,
        session,
        focus,
        ..
    }) = capture.take()
    else {
        tracing::debug!("release without active capture");
        return;
    };
    if let Some(released) = released {
        if released != mode {
            tracing::warn!("release {released:?} but captured {mode:?}; using captured mode");
        }
    }
    let cfg = ctx.cfg;
    if cfg.router.cues {
        cues::play(cues::Cue::Stop);
    }
    let samples = session.stop();
    if *ctx.lock_rx.borrow() {
        // Locked between press and release: whatever was said goes nowhere.
        tracing::info!("session locked during {mode:?} capture; discarding utterance");
        ctx.fail("session locked; utterance discarded");
        return;
    }
    if let Some(bus) = ctx.bus {
        bus.set_state(State::Transcribing, Some(mode));
    }
    match ctx.job_tx.try_send(Job {
        mode,
        samples,
        focus,
    }) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::error!("{JOB_QUEUE} utterances already waiting on ASR; dropping this one");
            ctx.fail("too many utterances waiting; dropped one");
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
    gate: Arc<Gate>,
    asr: Arc<Asr>,
    router: Arc<Router>,
    lock_rx: watch::Receiver<bool>,
    bus: Option<Bus>,
) {
    while let Some(Job {
        mode,
        samples,
        focus,
    }) = jobs.recv().await
    {
        let t0 = std::time::Instant::now();
        let focus = focus.await.unwrap_or_default();
        let focused = focus.window;
        let heard = match transcribe(samples, &cfg.audio, &gate, &asr, router.flow()).await {
            Ok(h) => h,
            Err(e) => {
                report(&cfg, bus.as_ref(), mode, Err(e));
                continue;
            }
        };
        // Last check before anything touches the desktop: transcription
        // takes long enough for the screen to have locked in the meantime.
        if *lock_rx.borrow() {
            tracing::info!("session locked; discarding transcribed {mode:?} utterance");
            report(&cfg, bus.as_ref(), mode, Err(anyhow::anyhow!("session locked; utterance discarded")));
            continue;
        }
        let result = router
            .handle(mode, &heard.transcript, focused.as_ref(), focus.text.as_ref())
            .await;
        let latency_ms = t0.elapsed().as_millis() as u64;
        let record = record_of(mode, &heard, focused.as_ref(), &result, latency_ms);
        if let Some(record) = router.flow().record(record) {
            if let Some(bus) = &bus {
                bus.notice(Notice::Utterance(Box::new(record)));
            }
        }
        let asked = matches!(result, Ok(Handled::Confirm(_)));
        report(&cfg, bus.as_ref(), mode, result);
        if let (Some(bus), true) = (&bus, asked) {
            // The prompt stays answerable for the confirm window; the UI
            // shows it that long unless something else happens first.
            let bus = bus.clone();
            let window = Duration::from_millis(cfg.router.confirm_window_ms);
            tokio::spawn(async move {
                tokio::time::sleep(window).await;
                if bus.state() == State::Waiting {
                    bus.finish("", false);
                }
            });
        }
    }
}

/// Tell the user and the UI what became of an utterance.
fn report(cfg: &DaemonConfig, bus: Option<&Bus>, mode: Mode, result: anyhow::Result<Handled>) {
    match result {
        Ok(Handled::Done(msg)) => {
            tracing::info!("{mode:?} done: {msg}");
            if cfg.router.notify_results {
                notify("parla", &msg);
            }
            if let Some(bus) = bus {
                bus.finish(&msg, false);
            }
        }
        Ok(Handled::Typed(p)) => {
            let words = parla_flow::text::word_count(&p.text);
            let msg = match p.outcome {
                "snippet" => "inserted a snippet".to_string(),
                "edited" => format!("rewrote it: {} words", words),
                _ => format!("typed {words} words"),
            };
            tracing::info!("{mode:?} {msg}");
            if let Some(bus) = bus {
                bus.finish(&msg, false);
            }
        }
        Ok(Handled::Confirm(question)) => {
            // The user has to hear this one whatever notify_results
            // says: the action is waiting on their answer.
            tracing::info!("{mode:?} waiting: {question}");
            if cfg.router.cues {
                cues::play(cues::Cue::Stop);
            }
            notify("parla", &question);
            if let Some(bus) = bus {
                bus.set_state(State::Waiting, None);
                bus.notice(Notice::Confirm(question));
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            tracing::warn!("{mode:?} failed: {msg}");
            if cfg.router.cues {
                cues::play(cues::Cue::Error);
            }
            if cfg.router.notify_results {
                notify("parla", &msg);
            }
            if let Some(bus) = bus {
                bus.finish(&msg, true);
                bus.notice(Notice::Error(msg));
            }
        }
    }
}

/// What whisper made of a capture.
struct Heard {
    transcript: String,
    audio_ms: u64,
}

async fn transcribe(
    samples: audio::Stopped,
    audio_cfg: &config::AudioConfig,
    gate: &Arc<Gate>,
    asr: &Arc<Asr>,
    flow: &Flow,
) -> anyhow::Result<Heard> {
    // The capture thread hands the samples over as soon as it closes the
    // stream; if PipeWire wedges that close, do not wedge the whole queue.
    let samples = tokio::time::timeout(STOP_GRACE, samples)
        .await
        .context("capture device did not stop in time")??;
    // The gate runs the VAD model on the CPU, so it shares the blocking
    // thread with whisper rather than stalling the runtime.
    let gate = Arc::clone(gate);
    let asr = Arc::clone(asr);
    let audio_cfg = audio_cfg.clone();
    let prompt = flow.asr_prompt();
    let (transcript, audio_ms) = tokio::task::spawn_blocking(move || {
        let trimmed = gate.validate(&samples, audio::SAMPLE_RATE, &audio_cfg)?;
        let audio_ms = trimmed.len() as u64 * 1000 / u64::from(audio::SAMPLE_RATE);
        let transcript = asr.transcribe(&trimmed, prompt.as_deref())?;
        anyhow::Ok((transcript, audio_ms))
    })
    .await
    .context("ASR task panicked")??;
    anyhow::ensure!(!transcript.is_empty(), "heard nothing usable");
    Ok(Heard {
        transcript,
        audio_ms,
    })
}

/// The history line for one utterance.
fn record_of(
    mode: Mode,
    heard: &Heard,
    focused: Option<&Window>,
    result: &anyhow::Result<Handled>,
    latency_ms: u64,
) -> Record {
    let (text, outcome, detail, profile) = match result {
        Ok(Handled::Typed(p)) => (p.text.clone(), p.outcome.to_string(), String::new(), p.profile.clone()),
        Ok(Handled::Done(msg)) => (String::new(), "command".to_string(), msg.clone(), String::new()),
        Ok(Handled::Confirm(q)) => (String::new(), "confirm".to_string(), q.clone(), String::new()),
        Err(e) => (String::new(), "error".to_string(), format!("{e:#}"), String::new()),
    };
    Record {
        mode: match mode {
            Mode::Dictate => "dictate",
            Mode::Command => "command",
        }
        .into(),
        app: focused.map(|w| w.class.clone()).unwrap_or_default(),
        title: focused.map(|w| w.title.clone()).unwrap_or_default(),
        raw: heard.transcript.clone(),
        words: parla_flow::text::word_count(if text.is_empty() {
            &heard.transcript
        } else {
            &text
        }),
        text,
        profile,
        audio_ms: heard.audio_ms,
        latency_ms,
        outcome,
        detail,
        ..Record::default()
    }
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
    anyhow::ensure!(cfg.judge.enabled, "judge.enabled = false in the config");
    let local = load_local(&cfg).await;
    let judge = Arc::new(judge::Judge::new(&cfg.judge, local)?);
    println!("judge:     {}", judge.describe());
    let executor = Arc::new(Executor::new(cfg.desktopd.clone()).await?);
    let mut flow_cfg = cfg.flow.clone();
    flow_cfg.cleanup = false;
    flow_cfg.history = false;
    let flow = Arc::new(Flow::load(&flow_cfg, None, None)?);
    let router = Router::new(
        Arc::clone(&executor),
        Arc::new(grammar),
        Some(Arc::clone(&judge)),
        flow,
        Duration::from_millis(cfg.router.confirm_window_ms),
        None,
    );
    let mut snapshot = router.snapshot(None).await?;
    // Pretend something was just dictated, to try the edit intents.
    snapshot.last_dictation = std::env::var_os("PARLA_LAST_DICTATION").is_some();
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
            if let parla_grammar::Intent::EditText { instruction } = &resolved.intent {
                match decision {
                    policy::Decision::Refuse { reason } => println!("would refuse: {reason}"),
                    _ => println!("would rewrite the last dictation as told: {instruction:?}"),
                }
                return Ok(());
            }
            let command = command::from_resolved(resolved).context("judge produced a reply")?;
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

/// `parlad --flow "<text>" [window class]`: run one transcript through the
/// dictation flow (snippets, dictionary, cleanup for that window class) and
/// print what would be typed, without typing it. The way to try a profile's
/// instructions or a dictionary entry. `PARLA_CONTEXT_BEFORE` in the
/// environment stands in for the text before the cursor, so the context
/// section of the prompt can be tried without a live field. With
/// `--edit "<text>" "<instruction>"` it applies a spoken edit to `text`
/// instead.
async fn flow_once(text: &str, instruction: Option<&str>, class: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !text.trim().is_empty() && instruction.is_none_or(|i| !i.trim().is_empty()),
        "usage: parlad --flow \"<transcript>\" [window class]\n       parlad --edit \"<text>\" \"<instruction>\" [window class]"
    );
    let cfg = DaemonConfig::load()?;
    let mut flow_cfg = cfg.flow.clone();
    flow_cfg.history = false;
    let local = if flow_cfg.cleanup && flow_cfg.backend == config::FlowBackend::Local {
        let local_cfg = cfg.local.clone();
        Some(Arc::new(
            tokio::task::spawn_blocking(move || LocalModel::load(&local_cfg))
                .await
                .context("model load task panicked")??,
        ))
    } else {
        None
    };
    let flow = Flow::load(&flow_cfg, cfg.asr.initial_prompt.clone(), local)?;
    let profile = flow.profile_for(class);
    println!("transcript: {text:?}");
    println!("window:     {:?} -> profile {:?} (tone {}, cleanup {})",
        class, profile.name, profile.tone.as_str(), if profile.cleanup { "on" } else { "off" });
    println!("cleanup:    {}", flow.describe());
    if let Some(p) = flow.asr_prompt() {
        println!("asr prompt: {p:?}");
    }
    let t0 = std::time::Instant::now();
    match instruction {
        Some(instruction) => {
            let out = flow.edit(text, instruction, class).await?;
            println!("edit:       {instruction:?} in {:.0}ms", t0.elapsed().as_secs_f64() * 1000.0);
            println!("would type: {out:?}");
        }
        None => {
            let context = std::env::var("PARLA_CONTEXT_BEFORE")
                .ok()
                .map(|before| TextContext {
                    app: "an application".into(),
                    role: "text field".into(),
                    before,
                });
            match &context {
                Some(c) => println!("context:    before the cursor {:?}", c.before),
                None => println!("context:    none (set PARLA_CONTEXT_BEFORE to try one)"),
            }
            let out = flow.process(text, class, context.as_ref()).await;
            println!("outcome:    {} in {:.0}ms", out.outcome, t0.elapsed().as_secs_f64() * 1000.0);
            println!("would type: {:?}", out.text);
        }
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
    println!("vad:           {}", Gate::load(&cfg.asr, &cfg.audio).describe());
    let path = &cfg.local.model_path;
    println!("local model:   {}", path.display());
    println!(
        "local present: {}",
        if path.exists() {
            "yes"
        } else {
            "NO — run scripts/fetch-model.sh"
        }
    );
    if cfg.judge.enabled {
        let t = cfg.judge.thresholds();
        println!("judge:         {} backend", cfg.judge.backend);
        println!(
            "thresholds:    min_confidence {:.2} act_unconfirmed_above {:.2} dictation {:.2} destructive {:.2}",
            t.min_confidence, t.act_unconfirmed_above, t.dictation_threshold, t.destructive_threshold
        );
    } else {
        println!("judge:         disabled");
    }
    if cfg.flow.cleanup {
        println!("cleanup:       {} backend", cfg.flow.backend);
    } else {
        println!("cleanup:       off");
    }
    for (what, path, count) in [
        ("dictionary", parla_flow::paths::dictionary(), parla_flow::Dictionary::load(&parla_flow::paths::dictionary()).map(|d| format!("{} words, {} replacements", d.words.len(), d.replacements.len()))),
        ("snippets", parla_flow::paths::snippets(), parla_flow::Snippets::load(&parla_flow::paths::snippets()).map(|s| format!("{} snippets", s.snippets.len()))),
        ("app profiles", parla_flow::paths::apps(), parla_flow::AppProfiles::load(&parla_flow::paths::apps()).map(|a| format!("{} profiles", a.apps.len()))),
    ] {
        match count {
            Ok(c) => println!("{what:<14} {c} ({}{})", path.display(), if path.exists() { "" } else { ", not written yet" }),
            Err(e) => println!("{what:<14} FAILED: {e:#}"),
        }
    }
    println!("history:       {}", if cfg.flow.history { parla_flow::paths::history().display().to_string() } else { "off".into() });
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
    let conn = desktopd::bus::session().await?;
    println!("session locked: {}", lock::is_locked(&conn).await);
    match A11y::status().await {
        Ok(enabled) => println!("a11y:          bus reachable, IsEnabled={enabled}"),
        Err(e) => println!("a11y:          unavailable ({e:#})"),
    }
    let owned = zbus::fdo::DBusProxy::new(&conn)
        .await?
        .name_has_owner(dbus::NAME.try_into()?)
        .await?;
    println!(
        "session bus:   {} {}",
        dbus::NAME,
        if owned { "is taken (a parlad is running)" } else { "is free" }
    );
    println!("all checks passed");
    Ok(())
}
