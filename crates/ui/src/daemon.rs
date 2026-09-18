//! The `Daemon` QML singleton: a mirror of parlad's D-Bus interface that QML
//! binds to. Properties follow the daemon's; methods are fire-and-forget
//! (their results arrive as signals) so the UI thread never waits on the
//! bus. The bus itself lives on a worker thread, see [`bus`].

pub mod bus;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;
use tokio::sync::mpsc::UnboundedSender;

use bus::{Command, Event, Props};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qml_singleton]
        /// Whether org.parla.Daemon has an owner on the session bus.
        #[qproperty(bool, connected)]
        /// Daemon properties, names as in org.parla.Daemon1.
        #[qproperty(QString, state)]
        #[qproperty(QString, mode)]
        #[qproperty(bool, enabled, READ, WRITE = request_enabled, NOTIFY)]
        #[qproperty(QString, last_result)]
        #[qproperty(bool, last_result_is_error)]
        #[qproperty(QString, version)]
        #[qproperty(QString, judge)]
        #[qproperty(QString, cleanup)]
        #[qproperty(QString, dictate_hotkey)]
        #[qproperty(QString, command_hotkey)]
        /// Microphone level 0..1, updated by the Level signal while recording.
        #[qproperty(f64, level)]
        /// The open confirmation question, "" when none.
        #[qproperty(QString, confirm_question)]
        /// JSON of the last Stats() / Paths() answers, "" until fetched.
        #[qproperty(QString, stats_json)]
        #[qproperty(QString, paths_json)]
        type Daemon = super::DaemonRust;

        #[qinvokable]
        fn start(self: Pin<&mut Daemon>, mode: &QString);
        #[qinvokable]
        fn stop(self: Pin<&mut Daemon>);
        #[qinvokable]
        fn cancel(self: Pin<&mut Daemon>);
        #[qinvokable]
        fn reload(self: Pin<&mut Daemon>);
        /// Answers with `historyLoaded(json, offset)`.
        #[qinvokable]
        fn fetch_history(self: Pin<&mut Daemon>, limit: u32, offset: u32);
        /// Answers with `historyDeleted(id)` when the record existed.
        #[qinvokable]
        fn delete_history(self: Pin<&mut Daemon>, id: &QString);
        #[qinvokable]
        fn clear_history(self: Pin<&mut Daemon>);
        /// Updates `statsJson`.
        #[qinvokable]
        fn fetch_stats(self: Pin<&mut Daemon>);
        /// Updates `pathsJson`.
        #[qinvokable]
        fn fetch_paths(self: Pin<&mut Daemon>);
        /// Answers with `previewReady(cleaned)`.
        #[qinvokable]
        fn preview(self: Pin<&mut Daemon>, text: &QString, app: &QString);
        /// The Enabled property setter: writes through to the daemon.
        fn request_enabled(self: Pin<&mut Daemon>, on: bool);

        #[qsignal]
        fn history_loaded(self: Pin<&mut Daemon>, json: QString, offset: u32);
        #[qsignal]
        fn history_deleted(self: Pin<&mut Daemon>, id: QString);
        #[qsignal]
        fn history_cleared(self: Pin<&mut Daemon>);
        #[qsignal]
        fn preview_ready(self: Pin<&mut Daemon>, cleaned: QString);
        /// A finished utterance as a Record JSON (the daemon's Utterance signal).
        #[qsignal]
        fn utterance(self: Pin<&mut Daemon>, record_json: QString);
        /// A message the user should see: daemon errors and bus failures.
        #[qsignal]
        fn error(self: Pin<&mut Daemon>, message: QString);
        #[qsignal]
        fn confirm(self: Pin<&mut Daemon>, question: QString);
        /// Reload succeeded (after a save) or failed with a message.
        #[qsignal]
        fn reloaded(self: Pin<&mut Daemon>, ok: bool, message: QString);
    }

    impl cxx_qt::Threading for Daemon {}
    impl cxx_qt::Initialize for Daemon {}
}

#[derive(Default)]
pub struct DaemonRust {
    connected: bool,
    state: QString,
    mode: QString,
    enabled: bool,
    last_result: QString,
    last_result_is_error: bool,
    version: QString,
    judge: QString,
    cleanup: QString,
    dictate_hotkey: QString,
    command_hotkey: QString,
    level: f64,
    confirm_question: QString,
    stats_json: QString,
    paths_json: QString,
    /// Commands for the bus worker; None until initialised.
    tx: Option<UnboundedSender<Command>>,
}

impl cxx_qt::Initialize for qobject::Daemon {
    fn initialize(mut self: core::pin::Pin<&mut Self>) {
        // Nothing is known until the worker reports in.
        self.as_mut().set_state(QString::from("unknown"));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.as_mut().rust_mut().tx = Some(tx);
        let qt = self.qt_thread();
        std::thread::Builder::new()
            .name("parla-ui-bus".into())
            .spawn(move || {
                bus::run(rx, move |event| {
                    // The QObject may be gone at shutdown; then there is nobody to tell.
                    let _ = qt.queue(move |daemon| daemon.apply(event));
                })
            })
            .expect("spawning the bus thread");
    }
}

impl qobject::Daemon {
    fn send(self: core::pin::Pin<&mut Self>, cmd: Command) {
        if let Some(tx) = &self.rust().tx {
            let _ = tx.send(cmd);
        }
    }

    pub fn start(self: core::pin::Pin<&mut Self>, mode: &QString) {
        self.send(Command::Start(mode.to_string()));
    }
    pub fn stop(self: core::pin::Pin<&mut Self>) {
        self.send(Command::Stop);
    }
    pub fn cancel(self: core::pin::Pin<&mut Self>) {
        self.send(Command::Cancel);
    }
    pub fn reload(self: core::pin::Pin<&mut Self>) {
        self.send(Command::Reload);
    }
    pub fn fetch_history(self: core::pin::Pin<&mut Self>, limit: u32, offset: u32) {
        self.send(Command::History { limit, offset });
    }
    pub fn delete_history(self: core::pin::Pin<&mut Self>, id: &QString) {
        self.send(Command::DeleteHistory(id.to_string()));
    }
    pub fn clear_history(self: core::pin::Pin<&mut Self>) {
        self.send(Command::ClearHistory);
    }
    pub fn fetch_stats(self: core::pin::Pin<&mut Self>) {
        self.send(Command::Stats);
    }
    pub fn fetch_paths(self: core::pin::Pin<&mut Self>) {
        self.send(Command::Paths);
    }
    pub fn preview(self: core::pin::Pin<&mut Self>, text: &QString, app: &QString) {
        self.send(Command::Preview {
            text: text.to_string(),
            app: app.to_string(),
        });
    }
    pub fn request_enabled(mut self: core::pin::Pin<&mut Self>, on: bool) {
        // Optimistic: the switch moves now, PropertiesChanged confirms or reverts.
        if self.rust().enabled != on {
            self.as_mut().rust_mut().enabled = on;
            self.as_mut().enabled_changed();
        }
        self.send(Command::SetEnabled(on));
    }

    /// Apply one event from the bus worker on the Qt thread.
    fn apply(mut self: core::pin::Pin<&mut Self>, event: Event) {
        match event {
            Event::Connected(on) => {
                self.as_mut().set_connected(on);
                if !on {
                    self.as_mut().set_state(QString::from("offline"));
                    self.as_mut().set_mode(QString::from(""));
                    self.as_mut().set_level(0.0);
                    self.as_mut().set_confirm_question(QString::from(""));
                }
            }
            Event::Props(props) => self.apply_props(props),
            Event::Level(level) => self.as_mut().set_level(level),
            Event::Utterance(json) => self.as_mut().utterance(QString::from(&json)),
            Event::Error(message) => self.as_mut().error(QString::from(&message)),
            Event::Confirm(question) => {
                self.as_mut().set_confirm_question(QString::from(&question));
                self.as_mut().confirm(QString::from(&question));
            }
            Event::History { json, offset } => self.as_mut().history_loaded(QString::from(&json), offset),
            Event::HistoryDeleted(id) => self.as_mut().history_deleted(QString::from(&id)),
            Event::HistoryCleared => self.as_mut().history_cleared(),
            Event::Stats(json) => self.as_mut().set_stats_json(QString::from(&json)),
            Event::Paths(json) => self.as_mut().set_paths_json(QString::from(&json)),
            Event::Preview(text) => self.as_mut().preview_ready(QString::from(&text)),
            Event::Reloaded(result) => match result {
                Ok(()) => self.as_mut().reloaded(true, QString::from("")),
                Err(e) => self.as_mut().reloaded(false, QString::from(&e)),
            },
        }
    }

    fn apply_props(mut self: core::pin::Pin<&mut Self>, p: Props) {
        if let Some(v) = p.state {
            if v != "waiting" {
                self.as_mut().set_confirm_question(QString::from(""));
            }
            if v == "recording" || v == "idle" {
                self.as_mut().set_level(0.0);
            }
            self.as_mut().set_state(QString::from(&v));
        }
        if let Some(v) = p.mode {
            self.as_mut().set_mode(QString::from(&v));
        }
        if let Some(v) = p.enabled {
            if self.rust().enabled != v {
                self.as_mut().rust_mut().enabled = v;
                self.as_mut().enabled_changed();
            }
        }
        if let Some(v) = p.last_result_is_error {
            self.as_mut().set_last_result_is_error(v);
        }
        if let Some(v) = p.last_result {
            // Force a change notification even for a repeated message, so the
            // overlay flashes again.
            if self.rust().last_result == QString::from(&v) {
                self.as_mut().set_last_result(QString::from(""));
            }
            self.as_mut().set_last_result(QString::from(&v));
        }
        if let Some(v) = p.version {
            self.as_mut().set_version(QString::from(&v));
        }
        if let Some(v) = p.judge {
            self.as_mut().set_judge(QString::from(&v));
        }
        if let Some(v) = p.cleanup {
            self.as_mut().set_cleanup(QString::from(&v));
        }
        if let Some(v) = p.dictate_hotkey {
            self.as_mut().set_dictate_hotkey(QString::from(&v));
        }
        if let Some(v) = p.command_hotkey {
            self.as_mut().set_command_hotkey(QString::from(&v));
        }
    }
}
