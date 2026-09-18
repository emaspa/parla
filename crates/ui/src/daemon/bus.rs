//! The session-bus side of the UI: a tokio worker that owns the connection,
//! follows org.parla.Daemon appearing and disappearing, forwards its signals
//! and property changes as [`Event`]s, and runs [`Command`]s from the UI.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt as _;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

pub const NAME: &str = "org.parla.Daemon";
pub const PATH: &str = "/org/parla/Daemon";
pub const INTERFACE: &str = "org.parla.Daemon1";

/// org.parla.Daemon1 as a zbus proxy. Properties are not cached: the worker
/// listens to PropertiesChanged itself and re-reads on reconnect.
#[zbus::proxy(
    interface = "org.parla.Daemon1",
    default_service = "org.parla.Daemon",
    default_path = "/org/parla/Daemon",
    gen_blocking = false
)]
pub trait Daemon {
    fn start(&self, mode: &str) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn cancel(&self) -> zbus::Result<()>;
    fn reload(&self) -> zbus::Result<()>;
    fn history(&self, limit: u32, offset: u32) -> zbus::Result<String>;
    fn delete_history(&self, id: &str) -> zbus::Result<bool>;
    fn clear_history(&self) -> zbus::Result<()>;
    fn stats(&self) -> zbus::Result<String>;
    fn paths(&self) -> zbus::Result<String>;
    fn preview(&self, text: &str, app: &str) -> zbus::Result<String>;

    #[zbus(property)]
    fn set_enabled(&self, on: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    fn state_changed(&self, state: String, mode: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn level(&self, level: f64) -> zbus::Result<()>;
    #[zbus(signal)]
    fn utterance(&self, record: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn error(&self, message: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn confirm(&self, question: String) -> zbus::Result<()>;
}

/// What the UI asks the daemon to do.
#[derive(Debug)]
pub enum Command {
    Start(String),
    Stop,
    Cancel,
    Reload,
    History { limit: u32, offset: u32 },
    DeleteHistory(String),
    ClearHistory,
    Stats,
    Paths,
    Preview { text: String, app: String },
    SetEnabled(bool),
}

/// Property values that changed; None means unchanged.
#[derive(Debug, Default)]
pub struct Props {
    pub state: Option<String>,
    pub mode: Option<String>,
    pub enabled: Option<bool>,
    pub last_result: Option<String>,
    pub last_result_is_error: Option<bool>,
    pub version: Option<String>,
    pub judge: Option<String>,
    pub cleanup: Option<String>,
    pub dictate_hotkey: Option<String>,
    pub command_hotkey: Option<String>,
}

impl Props {
    fn set(&mut self, name: &str, value: &Value<'_>) {
        let s = || match value {
            Value::Str(s) => Some(s.to_string()),
            _ => None,
        };
        let b = || match value {
            Value::Bool(b) => Some(*b),
            _ => None,
        };
        match name {
            "State" => self.state = s(),
            "Mode" => self.mode = s(),
            "Enabled" => self.enabled = b(),
            "LastResult" => self.last_result = s(),
            "LastResultIsError" => self.last_result_is_error = b(),
            "Version" => self.version = s(),
            "Judge" => self.judge = s(),
            "Cleanup" => self.cleanup = s(),
            "DictateHotkey" => self.dictate_hotkey = s(),
            "CommandHotkey" => self.command_hotkey = s(),
            _ => {}
        }
    }

    fn from_map(map: &HashMap<String, OwnedValue>) -> Self {
        let mut p = Props::default();
        for (k, v) in map {
            p.set(k, v);
        }
        p
    }
}

/// What the worker tells the UI.
#[derive(Debug)]
pub enum Event {
    Connected(bool),
    Props(Props),
    Level(f64),
    Utterance(String),
    Error(String),
    Confirm(String),
    History { json: String, offset: u32 },
    HistoryDeleted(String),
    HistoryCleared,
    Stats(String),
    Paths(String),
    Preview(String),
    Reloaded(Result<(), String>),
}

/// Run the worker until the command channel closes. `emit` is called from
/// this thread for every event.
pub fn run(rx: UnboundedReceiver<Command>, emit: impl Fn(Event) + Send + 'static) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime for the bus worker");
    rt.block_on(async move {
        let mut rx = rx;
        loop {
            match worker(&mut rx, &emit).await {
                Ok(()) => break,
                Err(e) => {
                    emit(Event::Connected(false));
                    emit(Event::Error(format!("session bus: {e:#}")));
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
    });
}

async fn worker(
    rx: &mut UnboundedReceiver<Command>,
    emit: &(impl Fn(Event) + Send + 'static),
) -> anyhow::Result<()> {
    let conn = Connection::session().await?;
    let dbus = DBusProxy::new(&conn).await?;
    let mut owner_changed = dbus
        .receive_name_owner_changed_with_args(&[(0, NAME)])
        .await?;
    let proxy = DaemonProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let properties = PropertiesProxy::builder(&conn)
        .destination(NAME)?
        .path(PATH)?
        .build()
        .await?;
    let mut props_changed = properties.receive_properties_changed().await?;
    let mut state_changed = proxy.receive_state_changed().await?;
    let mut level = proxy.receive_level().await?;
    let mut utterance = proxy.receive_utterance().await?;
    let mut error = proxy.receive_error().await?;
    let mut confirm = proxy.receive_confirm().await?;

    let mut connected = refresh(&properties, emit).await;
    emit(Event::Connected(connected));

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                if !connected {
                    // Let a Start from the tray tell the user why nothing happens.
                    if matches!(cmd, Command::Start(_) | Command::Stop | Command::Cancel | Command::SetEnabled(_) | Command::Reload) {
                        emit(Event::Error("parlad is not running".into()));
                    }
                    continue;
                }
                handle(&proxy, cmd, emit).await;
            }
            Some(change) = owner_changed.next() => {
                let now = change.args().map(|a| !a.new_owner().is_none()).unwrap_or(false);
                if now && !connected {
                    // Give a freshly started daemon a moment to export its object.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    connected = refresh(&properties, emit).await;
                } else {
                    connected = now;
                }
                emit(Event::Connected(connected));
            }
            Some(sig) = state_changed.next() => {
                if let Ok(args) = sig.args() {
                    let mut p = Props { state: Some(args.state.clone()), mode: Some(args.mode.clone()), ..Default::default() };
                    // LastResult is what the overlay flashes when a capture ends.
                    if args.state == "idle" {
                        if let Ok(v) = get_all(&properties).await {
                            let q = Props::from_map(&v);
                            p.last_result = q.last_result;
                            p.last_result_is_error = q.last_result_is_error;
                        }
                    }
                    emit(Event::Props(p));
                }
            }
            Some(sig) = level.next() => {
                if let Ok(args) = sig.args() {
                    emit(Event::Level(args.level.clamp(0.0, 1.0)));
                }
            }
            Some(sig) = utterance.next() => {
                if let Ok(args) = sig.args() {
                    emit(Event::Utterance(args.record.clone()));
                }
            }
            Some(sig) = error.next() => {
                if let Ok(args) = sig.args() {
                    emit(Event::Error(args.message.clone()));
                }
            }
            Some(sig) = confirm.next() => {
                if let Ok(args) = sig.args() {
                    emit(Event::Confirm(args.question.clone()));
                }
            }
            Some(sig) = props_changed.next() => {
                if let Ok(args) = sig.args() {
                    if args.interface_name.as_str() != INTERFACE {
                        continue;
                    }
                    let changed: HashMap<String, OwnedValue> = args
                        .changed_properties
                        .iter()
                        .filter_map(|(k, v)| OwnedValue::try_from(v.clone()).ok().map(|v| (k.to_string(), v)))
                        .collect();
                    if !changed.is_empty() {
                        emit(Event::Props(Props::from_map(&changed)));
                    }
                    if !args.invalidated_properties.is_empty() {
                        if let Ok(all) = get_all(&properties).await {
                            emit(Event::Props(Props::from_map(&all)));
                        }
                    }
                }
            }
        }
    }
}

async fn get_all(properties: &PropertiesProxy<'_>) -> zbus::Result<HashMap<String, OwnedValue>> {
    properties
        .get_all(zbus::names::InterfaceName::from_static_str_unchecked(INTERFACE))
        .await
        .map_err(zbus::Error::from)
}

/// Read every property; true when the daemon answered.
async fn refresh(properties: &PropertiesProxy<'_>, emit: &impl Fn(Event)) -> bool {
    match get_all(properties).await {
        Ok(all) => {
            emit(Event::Props(Props::from_map(&all)));
            true
        }
        Err(_) => false,
    }
}

async fn handle(proxy: &DaemonProxy<'_>, cmd: Command, emit: &impl Fn(Event)) {
    let fail = |what: &str, e: zbus::Error| emit(Event::Error(format!("{what}: {}", describe(e))));
    match cmd {
        Command::Start(mode) => {
            if let Err(e) = proxy.start(&mode).await {
                fail("Start", e);
            }
        }
        Command::Stop => {
            if let Err(e) = proxy.stop().await {
                fail("Stop", e);
            }
        }
        Command::Cancel => {
            if let Err(e) = proxy.cancel().await {
                fail("Cancel", e);
            }
        }
        Command::Reload => emit(Event::Reloaded(proxy.reload().await.map_err(describe))),
        Command::History { limit, offset } => match proxy.history(limit, offset).await {
            Ok(json) => emit(Event::History { json, offset }),
            Err(e) => fail("History", e),
        },
        Command::DeleteHistory(id) => match proxy.delete_history(&id).await {
            Ok(true) => emit(Event::HistoryDeleted(id)),
            Ok(false) => {}
            Err(e) => fail("DeleteHistory", e),
        },
        Command::ClearHistory => match proxy.clear_history().await {
            Ok(()) => emit(Event::HistoryCleared),
            Err(e) => fail("ClearHistory", e),
        },
        Command::Stats => match proxy.stats().await {
            Ok(json) => emit(Event::Stats(json)),
            Err(e) => fail("Stats", e),
        },
        Command::Paths => match proxy.paths().await {
            Ok(json) => emit(Event::Paths(json)),
            Err(e) => fail("Paths", e),
        },
        Command::Preview { text, app } => match proxy.preview(&text, &app).await {
            Ok(cleaned) => emit(Event::Preview(cleaned)),
            Err(e) => fail("Preview", e),
        },
        Command::SetEnabled(on) => {
            if let Err(e) = proxy.set_enabled(on).await {
                fail("Enabled", e);
            }
        }
    }
}

/// A D-Bus error as the user should read it: the daemon's message without
/// the error name, and a plain sentence when the name has no owner.
fn describe(e: zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, msg, _) => {
            if name.as_str().ends_with("ServiceUnknown") || name.as_str().ends_with("NameHasNoOwner") {
                "parlad is not running".to_string()
            } else {
                msg.unwrap_or_else(|| name.to_string())
            }
        }
        zbus::Error::FDO(inner) => inner.to_string(),
        other => other.to_string(),
    }
}
