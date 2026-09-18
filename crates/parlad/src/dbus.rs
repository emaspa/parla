//! parlad on the session bus, for the UI: `org.parla.Daemon` at
//! `/org/parla/Daemon`, interface `org.parla.Daemon1`. The contract is in
//! `dbus/org.parla.Daemon1.xml`.
//!
//! The daemon owns the pipeline. The bus exposes what it is doing (state,
//! microphone level, results), lets a client start and stop a capture as a
//! hotkey would, and answers history queries. The editable files are
//! written by the client itself, which then asks for a reload.
//!
//! Everything the main loop reports goes through [`Bus`], which works with
//! nobody listening: a session without a bus loses the UI, not dictation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parla_flow::Record;
use tokio::sync::{mpsc, oneshot, watch};
use zbus::object_server::SignalEmitter;

use crate::flow::Flow;
use crate::router::Mode;

pub const NAME: &str = "org.parla.Daemon";
pub const PATH: &str = "/org/parla/Daemon";

/// How often the microphone level is published while recording.
const LEVEL_INTERVAL: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording,
    Transcribing,
    Thinking,
    Typing,
    Waiting,
    Paused,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Recording => "recording",
            State::Transcribing => "transcribing",
            State::Thinking => "thinking",
            State::Typing => "typing",
            State::Waiting => "waiting",
            State::Paused => "paused",
        }
    }
}

fn mode_str(mode: Option<Mode>) -> &'static str {
    match mode {
        Some(Mode::Dictate) => "dictate",
        Some(Mode::Command) => "command",
        None => "",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub state: State,
    pub mode: Option<Mode>,
    pub last_result: String,
    pub is_error: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            state: State::Idle,
            mode: None,
            last_result: String::new(),
            is_error: false,
        }
    }
}

/// A client's request to the main loop, answered once it has been acted on.
#[derive(Debug)]
pub enum Control {
    Start(Mode, oneshot::Sender<anyhow::Result<()>>),
    Stop(oneshot::Sender<anyhow::Result<()>>),
    Cancel(oneshot::Sender<anyhow::Result<()>>),
    /// The Enabled property was written; the loop re-publishes its state.
    Enabled(bool),
}

/// Things worth a signal of their own.
#[derive(Debug)]
pub enum Notice {
    Utterance(Box<Record>),
    Error(String),
    Confirm(String),
}

/// The main loop's end: what it reports, and the switch it consults.
#[derive(Clone)]
pub struct Bus {
    status: watch::Sender<Status>,
    level: watch::Sender<f32>,
    notices: mpsc::UnboundedSender<Notice>,
    enabled: Arc<AtomicBool>,
}

impl Bus {
    pub fn set_state(&self, state: State, mode: Option<Mode>) {
        self.status.send_modify(|s| {
            s.state = state;
            s.mode = mode;
        });
    }

    /// Record an outcome and go idle.
    pub fn finish(&self, result: &str, is_error: bool) {
        self.status.send_modify(|s| {
            s.state = State::Idle;
            s.mode = None;
            s.last_result = result.to_string();
            s.is_error = is_error;
        });
    }

    pub fn state(&self) -> State {
        self.status.borrow().state
    }

    /// The microphone level sender for a capture to feed.
    pub fn level(&self) -> watch::Sender<f32> {
        self.level.clone()
    }

    pub fn notice(&self, notice: Notice) {
        let _ = self.notices.send(notice);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
}

/// Static facts the UI shows.
pub struct Info {
    pub version: String,
    pub judge: String,
    pub cleanup: String,
    pub dictate_hotkey: String,
    pub command_hotkey: String,
}

/// The interface object.
pub struct Daemon {
    status: watch::Receiver<Status>,
    enabled: Arc<AtomicBool>,
    control: mpsc::Sender<Control>,
    flow: Arc<Flow>,
    info: Info,
}

fn failed(e: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}

impl Daemon {
    async fn request(
        &self,
        make: impl FnOnce(oneshot::Sender<anyhow::Result<()>>) -> Control,
    ) -> zbus::fdo::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.control
            .send(make(tx))
            .await
            .map_err(|_| failed("the daemon's main loop is gone"))?;
        rx.await
            .map_err(|_| failed("the daemon did not answer"))?
            .map_err(|e| failed(format!("{e:#}")))
    }

    fn json<T: serde::Serialize>(v: &T) -> zbus::fdo::Result<String> {
        serde_json::to_string(v).map_err(failed)
    }
}

#[zbus::interface(name = "org.parla.Daemon1")]
impl Daemon {
    #[zbus(property)]
    fn state(&self) -> String {
        self.status.borrow().state.as_str().into()
    }

    #[zbus(property)]
    fn mode(&self) -> String {
        mode_str(self.status.borrow().mode).into()
    }

    #[zbus(property)]
    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    #[zbus(property)]
    async fn set_enabled(&mut self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        let _ = self.control.send(Control::Enabled(enabled)).await;
    }

    #[zbus(property)]
    fn last_result(&self) -> String {
        self.status.borrow().last_result.clone()
    }

    #[zbus(property)]
    fn last_result_is_error(&self) -> bool {
        self.status.borrow().is_error
    }

    #[zbus(property)]
    fn version(&self) -> String {
        self.info.version.clone()
    }

    #[zbus(property)]
    fn judge(&self) -> String {
        self.info.judge.clone()
    }

    #[zbus(property)]
    fn cleanup(&self) -> String {
        self.info.cleanup.clone()
    }

    #[zbus(property)]
    fn dictate_hotkey(&self) -> String {
        self.info.dictate_hotkey.clone()
    }

    #[zbus(property)]
    fn command_hotkey(&self) -> String {
        self.info.command_hotkey.clone()
    }

    async fn start(&self, mode: &str) -> zbus::fdo::Result<()> {
        let mode = match mode {
            "dictate" => Mode::Dictate,
            "command" => Mode::Command,
            other => return Err(zbus::fdo::Error::InvalidArgs(format!("mode {other:?}"))),
        };
        self.request(|tx| Control::Start(mode, tx)).await
    }

    async fn stop(&self) -> zbus::fdo::Result<()> {
        self.request(Control::Stop).await
    }

    async fn cancel(&self) -> zbus::fdo::Result<()> {
        self.request(Control::Cancel).await
    }

    fn reload(&self) -> zbus::fdo::Result<()> {
        self.flow.reload().map_err(|e| failed(format!("{e:#}")))
    }

    fn history(&self, limit: u32, offset: u32) -> zbus::fdo::Result<String> {
        let Some(history) = self.flow.history() else {
            return Ok("[]".into());
        };
        let records = history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recent(limit as usize, offset as usize)
            .map_err(|e| failed(format!("{e:#}")))?;
        Self::json(&records)
    }

    fn delete_history(&self, id: &str) -> zbus::fdo::Result<bool> {
        let Some(history) = self.flow.history() else {
            return Ok(false);
        };
        history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .delete(id)
            .map_err(|e| failed(format!("{e:#}")))
    }

    fn clear_history(&self) -> zbus::fdo::Result<()> {
        let Some(history) = self.flow.history() else {
            return Ok(());
        };
        history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear()
            .map_err(|e| failed(format!("{e:#}")))
    }

    fn stats(&self) -> zbus::fdo::Result<String> {
        let stats = match self.flow.history() {
            Some(h) => h
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .stats()
                .map_err(|e| failed(format!("{e:#}")))?,
            None => parla_flow::Stats::default(),
        };
        Self::json(&stats)
    }

    fn paths(&self) -> zbus::fdo::Result<String> {
        Self::json(&self.flow.paths())
    }

    async fn preview(&self, text: &str, app: &str) -> zbus::fdo::Result<String> {
        Ok(self.flow.process(text, app).await.text)
    }

    #[zbus(signal, name = "StateChanged")]
    async fn status_changed(
        emitter: &SignalEmitter<'_>,
        state: &str,
        mode: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn level(emitter: &SignalEmitter<'_>, level: f64) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn utterance(emitter: &SignalEmitter<'_>, record: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn error(emitter: &SignalEmitter<'_>, message: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn confirm(emitter: &SignalEmitter<'_>, question: &str) -> zbus::Result<()>;
}

/// Claim the name, serve the object, and publish changes until the [`Bus`]
/// is dropped. Returns the main loop's [`Bus`] and the control receiver.
pub async fn serve(
    conn: &zbus::Connection,
    flow: Arc<Flow>,
    info: Info,
) -> anyhow::Result<(Bus, mpsc::Receiver<Control>)> {
    serve_as(conn, NAME, flow, info).await
}

async fn serve_as(
    conn: &zbus::Connection,
    name: &str,
    flow: Arc<Flow>,
    info: Info,
) -> anyhow::Result<(Bus, mpsc::Receiver<Control>)> {
    let (status_tx, status_rx) = watch::channel(Status::default());
    let (level_tx, level_rx) = watch::channel(0.0f32);
    let (notice_tx, notice_rx) = mpsc::unbounded_channel();
    let (control_tx, control_rx) = mpsc::channel(8);
    let enabled = Arc::new(AtomicBool::new(true));
    let daemon = Daemon {
        status: status_rx.clone(),
        enabled: Arc::clone(&enabled),
        control: control_tx,
        flow,
        info,
    };
    conn.object_server().at(PATH, daemon).await?;
    conn.request_name(name).await?;
    let iface = conn.object_server().interface::<_, Daemon>(PATH).await?;
    tokio::spawn(publish(iface, status_rx, level_rx, notice_rx));
    Ok((
        Bus {
            status: status_tx,
            level: level_tx,
            notices: notice_tx,
            enabled,
        },
        control_rx,
    ))
}

async fn publish(
    iface: zbus::object_server::InterfaceRef<Daemon>,
    mut status: watch::Receiver<Status>,
    level: watch::Receiver<f32>,
    mut notices: mpsc::UnboundedReceiver<Notice>,
) {
    let emitter = iface.signal_emitter().clone();
    let mut ticker = tokio::time::interval(LEVEL_INTERVAL);
    let mut previous = Status::default();
    loop {
        tokio::select! {
            changed = status.changed() => {
                if changed.is_err() {
                    return;
                }
                let now = status.borrow_and_update().clone();
                let daemon = iface.get().await;
                if now.state != previous.state || now.mode != previous.mode {
                    let _ = Daemon::status_changed(&emitter, now.state.as_str(), mode_str(now.mode)).await;
                    let _ = daemon.state_changed(&emitter).await;
                    let _ = daemon.mode_changed(&emitter).await;
                }
                if now.last_result != previous.last_result || now.is_error != previous.is_error {
                    let _ = daemon.last_result_changed(&emitter).await;
                    let _ = daemon.last_result_is_error_changed(&emitter).await;
                }
                previous = now;
            }
            notice = notices.recv() => {
                let Some(notice) = notice else { return };
                let _ = match notice {
                    Notice::Utterance(r) => match serde_json::to_string(&*r) {
                        Ok(json) => Daemon::utterance(&emitter, &json).await,
                        Err(_) => Ok(()),
                    },
                    Notice::Error(m) => Daemon::error(&emitter, &m).await,
                    Notice::Confirm(q) => Daemon::confirm(&emitter, &q).await,
                };
            }
            _ = ticker.tick() => {
                if previous.state == State::Recording {
                    let v = f64::from(*level.borrow()).clamp(0.0, 1.0);
                    let _ = Daemon::level(&emitter, v).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    /// The interface as a client sees it: properties, methods, signals.
    /// Needs a session bus, as the daemon does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn round_trip_over_the_session_bus() {
        let tmp = std::env::temp_dir().join(format!("parla-dbus-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // The flow reads its files from XDG dirs; point them somewhere empty.
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        std::env::set_var("XDG_DATA_HOME", &tmp);
        let cfg = crate::config::FlowConfig {
            cleanup: false,
            ..Default::default()
        };
        let flow = Arc::new(Flow::load(&cfg, None, None).unwrap());

        let Ok(server) = zbus::Connection::session().await else {
            eprintln!("no session bus; skipping");
            return;
        };
        let name = format!("org.parla.Test{}", std::process::id());
        let info = Info {
            version: "test".into(),
            judge: "local test-model".into(),
            cleanup: String::new(),
            dictate_hotkey: "ctrl+space".into(),
            command_hotkey: "ctrl+shift+space".into(),
        };
        let (bus, mut control) = serve_as(&server, &name, flow, info).await.unwrap();

        let client = zbus::Connection::session().await.unwrap();
        // No property cache: the test reads properties right after a signal,
        // before the PropertiesChanged that would refresh a cache.
        let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&client)
            .destination(name.as_str())
            .unwrap()
            .path(PATH)
            .unwrap()
            .interface("org.parla.Daemon1")
            .unwrap()
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        assert_eq!(proxy.get_property::<String>("State").await.unwrap(), "idle");
        assert_eq!(
            proxy.get_property::<String>("Judge").await.unwrap(),
            "local test-model"
        );
        assert!(proxy.get_property::<bool>("Enabled").await.unwrap());

        let paths: serde_json::Value =
            serde_json::from_str(&proxy.call::<_, _, String>("Paths", &()).await.unwrap()).unwrap();
        assert!(paths["dictionary"]
            .as_str()
            .unwrap()
            .ends_with("dictionary.toml"));
        let stats: parla_flow::Stats =
            serde_json::from_str(&proxy.call::<_, _, String>("Stats", &()).await.unwrap()).unwrap();
        assert_eq!(stats.days.len(), 30);
        assert_eq!(
            proxy
                .call::<_, _, String>("History", &(10u32, 0u32))
                .await
                .unwrap(),
            "[]"
        );
        assert_eq!(
            proxy
                .call::<_, _, String>("Preview", &("hello there", "konsole"))
                .await
                .unwrap(),
            "hello there"
        );
        assert!(proxy
            .call::<_, _, ()>("Start", &("sideways",))
            .await
            .is_err());

        // A Start request reaches the main loop and its answer comes back.
        let answer = tokio::spawn(async move {
            match control.recv().await {
                Some(Control::Start(Mode::Command, reply)) => {
                    let _ = reply.send(Err(anyhow::anyhow!("the session is locked")));
                }
                other => panic!("unexpected control {other:?}"),
            }
            control
        });
        let err = proxy
            .call::<_, _, ()>("Start", &("command",))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("the session is locked"), "{err}");
        let mut control = answer.await.unwrap();

        // Writing Enabled flips the switch the loop reads and tells the loop.
        proxy.set_property("Enabled", false).await.unwrap();
        assert!(!bus.enabled());
        assert!(matches!(
            control.recv().await,
            Some(Control::Enabled(false))
        ));

        // State changes arrive as the StateChanged signal.
        let mut changes = proxy.receive_signal("StateChanged").await.unwrap();
        bus.set_state(State::Recording, Some(Mode::Dictate));
        let msg = tokio::time::timeout(Duration::from_secs(5), changes.next())
            .await
            .expect("signal in time")
            .unwrap();
        let (state, mode): (String, String) = msg.body().deserialize().unwrap();
        assert_eq!((state.as_str(), mode.as_str()), ("recording", "dictate"));
        assert_eq!(
            proxy.get_property::<String>("State").await.unwrap(),
            "recording"
        );

        let mut errors = proxy.receive_signal("Error").await.unwrap();
        bus.finish("boom", true);
        bus.notice(Notice::Error("boom".into()));
        let msg = tokio::time::timeout(Duration::from_secs(5), errors.next())
            .await
            .expect("signal in time")
            .unwrap();
        let (text,): (String,) = msg.body().deserialize().unwrap();
        assert_eq!(text, "boom");
        assert!(proxy
            .get_property::<bool>("LastResultIsError")
            .await
            .unwrap());
        std::fs::remove_dir_all(tmp).ok();
    }
}
