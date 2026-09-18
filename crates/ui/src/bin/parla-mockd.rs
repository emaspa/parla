//! parla-mockd: a stand-in for parlad on the session bus, for developing
//! and demoing the UI without a microphone, models or the real daemon.
//!
//! It owns org.parla.Daemon and implements all of org.parla.Daemon1 with a
//! demo loop: every few seconds it pretends to record (Level at 25 Hz),
//! transcribe, think and type, then emits an Utterance; now and then it
//! asks for a confirmation or fails. Start/Stop/Cancel work as they would.
//! It refuses to run when the real daemon already owns the name.

// Links the UI library so the crate's generated Qt code resolves; see lib.rs.
extern crate parla_ui;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use parla_flow::history::stats_of;
use parla_flow::{paths, Record};
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::{connection, interface};

const NAME: &str = "org.parla.Daemon";
const PATH: &str = "/org/parla/Daemon";

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle { until: Instant },
    /// `auto` captures end on their own; a Start()ed one waits for Stop().
    Recording { auto: bool, until: Instant, t: f64 },
    Transcribing { until: Instant },
    Thinking { until: Instant },
    Waiting { until: Instant },
    Typing { until: Instant },
}

struct Shared {
    phase: Phase,
    mode: String,
    enabled: bool,
    last_result: String,
    last_result_is_error: bool,
    history: Vec<Record>,
    counter: u64,
    cycle: u32,
    rng: u64,
}

impl Shared {
    fn state(&self) -> &'static str {
        if !self.enabled {
            return "paused";
        }
        match self.phase {
            Phase::Idle { .. } => "idle",
            Phase::Recording { .. } => "recording",
            Phase::Transcribing { .. } => "transcribing",
            Phase::Thinking { .. } => "thinking",
            Phase::Waiting { .. } => "waiting",
            Phase::Typing { .. } => "typing",
        }
    }

    fn rand(&mut self) -> f64 {
        // xorshift64*: enough for a demo.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        let i = (self.rand() * items.len() as f64) as usize;
        items[i.min(items.len() - 1)]
    }

    fn fabricate(&mut self, mode: &str, at_ms: u64) -> Record {
        const TEXTS: &[&str] = &[
            "Let's move the meeting to Thursday afternoon, I have a conflict in the morning.",
            "The build fails on the release profile because thin LTO drops the plugin symbols.",
            "Can you send me the link to the design doc when you get a chance?",
            "git commit -m \"parlad: confirm destructive commands by voice\"",
            "Thanks for the review, I'll address the comments tonight.",
            "Remind me to buy coffee beans and a new filter tomorrow.",
            "The overlay should hide two seconds after the result flashes.",
            "Sounds good to me, let's ship it.",
        ];
        const APPS: &[(&str, &str, &str)] = &[
            ("org.kde.konsole", "fish — konsole", "Terminals"),
            ("firefox", "Mozilla Firefox", ""),
            ("thunderbird", "Inbox — Thunderbird", "Mail and documents"),
            ("Slack", "Slack — parla", "Chat"),
            ("org.kde.kate", "config.rs — Kate", "Code editors"),
        ];
        let text = self.pick(TEXTS).to_string();
        let i = (self.rand() * APPS.len() as f64) as usize;
        let (app, title, profile) = APPS[i.min(APPS.len() - 1)];
        let (outcome, detail, text, raw) = if mode == "command" {
            let cmd = self.pick(&["Launched Firefox", "Closed Kate", "Muted the microphone", "Opened ~/Downloads"]);
            (
                "command",
                cmd.to_string(),
                cmd.to_lowercase(),
                cmd.to_lowercase(),
            )
        } else {
            ("typed", String::new(), text.clone(), text.to_lowercase().replace(['.', ','], ""))
        };
        let words = text.split_whitespace().count() as u32;
        self.counter += 1;
        Record {
            id: format!("{at_ms:x}-{:x}", self.counter),
            at_ms,
            mode: mode.to_string(),
            app: app.to_string(),
            title: title.to_string(),
            raw,
            text,
            profile: profile.to_string(),
            words,
            audio_ms: 1500 + (words as u64) * 380,
            latency_ms: 400 + (self.rand() * 900.0) as u64,
            outcome: outcome.to_string(),
            detail,
        }
    }
}

struct Mock {
    s: Arc<Mutex<Shared>>,
}

impl Mock {
    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.s.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[interface(name = "org.parla.Daemon1")]
impl Mock {
    #[zbus(property)]
    fn state(&self) -> String {
        self.lock().state().to_string()
    }
    #[zbus(property)]
    fn mode(&self) -> String {
        let s = self.lock();
        if matches!(s.phase, Phase::Idle { .. }) {
            String::new()
        } else {
            s.mode.clone()
        }
    }
    #[zbus(property)]
    fn enabled(&self) -> bool {
        self.lock().enabled
    }
    #[zbus(property)]
    async fn set_enabled(&mut self, on: bool, #[zbus(signal_emitter)] em: SignalEmitter<'_>) {
        let (changed, state, mode) = {
            let mut s = self.lock();
            let changed = s.enabled != on;
            s.enabled = on;
            if !on {
                s.phase = Phase::Idle { until: Instant::now() + Duration::from_secs(3) };
            }
            (changed, s.state().to_string(), String::new())
        };
        if changed {
            let _ = self.state_changed(&em).await;
            let _ = Mock::state_changed_signal(&em, &state, &mode).await;
        }
    }
    #[zbus(property)]
    fn last_result(&self) -> String {
        self.lock().last_result.clone()
    }
    #[zbus(property)]
    fn last_result_is_error(&self) -> bool {
        self.lock().last_result_is_error
    }
    #[zbus(property)]
    fn version(&self) -> String {
        format!("{}-mock", env!("CARGO_PKG_VERSION"))
    }
    #[zbus(property)]
    fn judge(&self) -> String {
        "local Qwen3-4B-Instruct-2507-Q4_K_M".into()
    }
    #[zbus(property)]
    fn cleanup(&self) -> String {
        "local Qwen3-4B-Instruct-2507-Q4_K_M".into()
    }
    #[zbus(property)]
    fn dictate_hotkey(&self) -> String {
        "ctrl+space".into()
    }
    #[zbus(property)]
    fn command_hotkey(&self) -> String {
        "ctrl+shift+space".into()
    }

    async fn start(&self, mode: &str, #[zbus(signal_emitter)] em: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        if mode != "dictate" && mode != "command" {
            return Err(zbus::fdo::Error::InvalidArgs(format!("unknown mode {mode:?}")));
        }
        {
            let mut s = self.lock();
            if !s.enabled {
                return Err(zbus::fdo::Error::Failed("parla is disabled".into()));
            }
            if !matches!(s.phase, Phase::Idle { .. }) {
                return Err(zbus::fdo::Error::Failed("a capture is already running".into()));
            }
            s.mode = mode.to_string();
            s.phase = Phase::Recording { auto: false, until: Instant::now() + Duration::from_secs(30), t: 0.0 };
        }
        let _ = Mock::state_changed_signal(&em, "recording", mode).await;
        Ok(())
    }

    async fn stop(&self, #[zbus(signal_emitter)] em: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        let mode = {
            let mut s = self.lock();
            if !matches!(s.phase, Phase::Recording { .. }) {
                return Err(zbus::fdo::Error::Failed("nothing is recording".into()));
            }
            s.phase = Phase::Transcribing { until: Instant::now() + Duration::from_millis(1200) };
            s.mode.clone()
        };
        let _ = Mock::state_changed_signal(&em, "transcribing", &mode).await;
        Ok(())
    }

    async fn cancel(&self, #[zbus(signal_emitter)] em: SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        {
            let mut s = self.lock();
            if matches!(s.phase, Phase::Idle { .. }) {
                return Err(zbus::fdo::Error::Failed("nothing to cancel".into()));
            }
            s.phase = Phase::Idle { until: Instant::now() + Duration::from_secs(8) };
        }
        let _ = Mock::state_changed_signal(&em, "idle", "").await;
        Ok(())
    }

    fn reload(&self) -> zbus::fdo::Result<()> {
        // Parse the files like the daemon would, so a broken save is reported.
        parla_flow::Dictionary::load(&paths::dictionary()).map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
        parla_flow::Snippets::load(&paths::snippets()).map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
        parla_flow::AppProfiles::load(&paths::apps()).map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
        eprintln!("parla-mockd: reloaded");
        Ok(())
    }

    fn history(&self, limit: u32, offset: u32) -> String {
        let s = self.lock();
        let page: Vec<&Record> = s.history.iter().rev().skip(offset as usize).take(limit as usize).collect();
        serde_json::to_string(&page).unwrap_or_else(|_| "[]".into())
    }

    fn delete_history(&self, id: &str) -> bool {
        let mut s = self.lock();
        let before = s.history.len();
        s.history.retain(|r| r.id != id);
        s.history.len() != before
    }

    fn clear_history(&self) {
        self.lock().history.clear();
    }

    fn stats(&self) -> String {
        let s = self.lock();
        serde_json::to_string(&stats_of(&s.history, Record::now_ms())).unwrap_or_else(|_| "{}".into())
    }

    fn paths(&self) -> String {
        serde_json::json!({
            "config": paths::config_dir().join("parla.toml"),
            "dictionary": paths::dictionary(),
            "snippets": paths::snippets(),
            "apps": paths::apps(),
            "history": paths::history(),
        })
        .to_string()
    }

    async fn preview(&self, text: &str, app: &str) -> String {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let profile = parla_flow::AppProfiles::load(&paths::apps())
            .map(|p| p.for_class(app).name)
            .unwrap_or_default();
        format!("{} (preview, profile {profile:?})", text.to_uppercase())
    }

    #[zbus(signal, name = "StateChanged")]
    async fn state_changed_signal(em: &SignalEmitter<'_>, state: &str, mode: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn level(em: &SignalEmitter<'_>, level: f64) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn utterance(em: &SignalEmitter<'_>, record: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn error(em: &SignalEmitter<'_>, message: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn confirm(em: &SignalEmitter<'_>, question: &str) -> zbus::Result<()>;
}

/// Seed history: a month of plausible dictation.
fn seed(s: &mut Shared) {
    let now = Record::now_ms();
    let day = 86_400_000u64;
    for back in (0..30u64).rev() {
        let n = if back % 7 >= 5 { 1 } else { 3 + (s.rand() * 6.0) as u64 };
        for k in 0..n {
            let at = now - back * day - k * 1_800_000 - (s.rand() * 600_000.0) as u64;
            let mode = if s.rand() < 0.15 { "command" } else { "dictate" };
            let r = s.fabricate(mode, at);
            s.history.push(r);
        }
    }
    s.history.sort_by_key(|r| r.at_ms);
}

async fn demo_loop(iface: InterfaceRef<Mock>, shared: Arc<Mutex<Shared>>) {
    use MockSignals as _;
    let mut tick = tokio::time::interval(Duration::from_millis(40));
    loop {
        tick.tick().await;
        let now = Instant::now();
        // Decide the transition under the lock, emit after releasing it.
        enum Out {
            None,
            Level(f64),
            State(&'static str, String),
            Confirm(String),
            Finish(Box<Record>, bool),
        }
        let out = {
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            if !s.enabled {
                Out::None
            } else {
                match s.phase.clone() {
                    Phase::Idle { until } if now >= until => {
                        s.cycle += 1;
                        s.mode = if s.cycle.is_multiple_of(3) { "command".into() } else { "dictate".into() };
                        s.phase = Phase::Recording { auto: true, until: now + Duration::from_millis(3200), t: 0.0 };
                        Out::State("recording", s.mode.clone())
                    }
                    Phase::Recording { auto, until, t } => {
                        if auto && now >= until {
                            s.phase = Phase::Transcribing { until: now + Duration::from_millis(1200) };
                            Out::State("transcribing", s.mode.clone())
                        } else {
                            // A wandering level: slow envelope plus jitter.
                            let t = t + 0.04;
                            let envelope = 0.35 + 0.3 * (t * 1.7).sin() + 0.15 * (t * 5.3).sin();
                            let level = (envelope + (s.rand() - 0.5) * 0.25).clamp(0.02, 1.0);
                            s.phase = Phase::Recording { auto, until, t };
                            Out::Level(level)
                        }
                    }
                    Phase::Transcribing { until } if now >= until => {
                        s.phase = Phase::Thinking { until: now + Duration::from_millis(1000) };
                        Out::State("thinking", s.mode.clone())
                    }
                    Phase::Thinking { until } if now >= until => {
                        if s.mode == "command" && s.cycle.is_multiple_of(6) {
                            s.phase = Phase::Waiting { until: now + Duration::from_millis(3500) };
                            Out::Confirm("Close 'config.rs — Kate'? say yes".into())
                        } else {
                            s.phase = Phase::Typing { until: now + Duration::from_millis(800) };
                            Out::State("typing", s.mode.clone())
                        }
                    }
                    Phase::Waiting { until } if now >= until => {
                        s.phase = Phase::Typing { until: now + Duration::from_millis(800) };
                        Out::State("typing", s.mode.clone())
                    }
                    Phase::Typing { until } if now >= until => {
                        s.phase = Phase::Idle { until: now + Duration::from_secs(8) };
                        let failed = s.cycle.is_multiple_of(5);
                        let mode = s.mode.clone();
                        let mut r = s.fabricate(&mode, Record::now_ms());
                        if failed {
                            r.outcome = "error".into();
                            r.detail = "the window lost focus while typing".into();
                            s.last_result = r.detail.clone();
                            s.last_result_is_error = true;
                        } else {
                            s.last_result = if r.outcome == "command" { r.detail.clone() } else { format!("typed {} words", r.words) };
                            s.last_result_is_error = false;
                        }
                        s.history.push(r.clone());
                        Out::Finish(Box::new(r), failed)
                    }
                    _ => Out::None,
                }
            }
        };
        let em = iface.signal_emitter();
        match out {
            Out::None => {}
            Out::Level(v) => {
                let _ = iface.level(v).await;
            }
            Out::State(state, mode) => {
                let _ = Mock::state_changed_signal(em, state, &mode).await;
            }
            Out::Confirm(q) => {
                let mode = shared.lock().map(|s| s.mode.clone()).unwrap_or_default();
                let _ = Mock::state_changed_signal(em, "waiting", &mode).await;
                let _ = iface.confirm(&q).await;
            }
            Out::Finish(r, failed) => {
                {
                    let guard = iface.get().await;
                    let _ = guard.last_result_changed(em).await;
                    let _ = guard.last_result_is_error_changed(em).await;
                }
                let json = serde_json::to_string(&r).unwrap_or_default();
                let _ = iface.utterance(&json).await;
                if failed {
                    let _ = iface.error(&r.detail).await;
                }
                let _ = Mock::state_changed_signal(em, "idle", "").await;
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut shared = Shared {
        phase: Phase::Idle { until: Instant::now() + Duration::from_secs(4) },
        mode: String::new(),
        enabled: true,
        last_result: String::new(),
        last_result_is_error: false,
        history: Vec::new(),
        counter: 0,
        cycle: 0,
        rng: 0x9E37_79B9_7F4A_7C15 ^ Record::now_ms(),
    };
    seed(&mut shared);
    let shared = Arc::new(Mutex::new(shared));

    let conn = connection::Builder::session()?
        .serve_at(PATH, Mock { s: shared.clone() })?
        .build()
        .await?;
    match conn
        .request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner) => {}
        Ok(_) | Err(zbus::Error::NameTaken) => {
            eprintln!("parla-mockd: {NAME} is already owned (parlad is running?), refusing to start");
            std::process::exit(1);
        }
        Err(e) => return Err(e.into()),
    }
    eprintln!("parla-mockd: serving {NAME} at {PATH}; ctrl-c to stop");
    let iface = conn.object_server().interface::<_, Mock>(PATH).await?;
    tokio::select! {
        _ = demo_loop(iface, shared) => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
