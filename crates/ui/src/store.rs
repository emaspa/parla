//! The `Store` QML singleton: the three editable TOML files (dictionary,
//! snippets, app profiles) as JSON for QML, saved through the parla-flow
//! types so the daemon and a text editor see the same format. Also the
//! desktop bits that are not the daemon's business: autostart and paths.

use std::path::PathBuf;

use cxx_qt::CxxQtType;
use cxx_qt_lib::QString;
use parla_flow::{paths, AppProfiles, Dictionary, Snippets};

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
        #[qproperty(QString, config_dir)]
        #[qproperty(QString, data_dir)]
        #[qproperty(QString, config_file)]
        #[qproperty(QString, dictionary_file)]
        #[qproperty(QString, snippets_file)]
        #[qproperty(QString, apps_file)]
        #[qproperty(QString, history_file)]
        #[qproperty(QString, autostart_file)]
        /// The last load/save failure, "" after a success.
        #[qproperty(QString, last_error)]
        #[qproperty(bool, autostart, READ, WRITE = set_autostart, NOTIFY)]
        type Store = super::StoreRust;

        /// JSON `{ "words": [..], "replace": [{spoken, written}] }`.
        #[qinvokable]
        fn load_dictionary(self: Pin<&mut Store>) -> QString;
        #[qinvokable]
        fn save_dictionary(self: Pin<&mut Store>, json: &QString) -> bool;
        /// JSON `{ "snippet": [{trigger, text}] }`.
        #[qinvokable]
        fn load_snippets(self: Pin<&mut Store>) -> QString;
        #[qinvokable]
        fn save_snippets(self: Pin<&mut Store>, json: &QString) -> bool;
        /// JSON `{ "app": [{name, class: [..], tone, cleanup, instructions}] }`.
        #[qinvokable]
        fn load_apps(self: Pin<&mut Store>) -> QString;
        #[qinvokable]
        fn save_apps(self: Pin<&mut Store>, json: &QString) -> bool;
        /// Open a file with the desktop's default application (xdg-open).
        #[qinvokable]
        fn open_in_editor(self: Pin<&mut Store>, path: &QString) -> bool;
        fn set_autostart(self: Pin<&mut Store>, on: bool);
    }

    impl cxx_qt::Initialize for Store {}
}

#[derive(Default)]
pub struct StoreRust {
    config_dir: QString,
    data_dir: QString,
    config_file: QString,
    dictionary_file: QString,
    snippets_file: QString,
    apps_file: QString,
    history_file: QString,
    autostart_file: QString,
    last_error: QString,
    autostart: bool,
}

fn autostart_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".config"));
    base.join("autostart").join("parla-ui.desktop")
}

const AUTOSTART_ENTRY: &str = "[Desktop Entry]
Type=Application
Name=parla
Comment=Voice dictation and commands
Exec=parla-ui --hidden
Icon=audio-input-microphone
Terminal=false
X-KDE-autostart-after=panel
X-KDE-StartupNotify=false
";

impl cxx_qt::Initialize for qobject::Store {
    fn initialize(mut self: core::pin::Pin<&mut Self>) {
        let q = |p: PathBuf| QString::from(p.to_string_lossy().as_ref());
        self.as_mut().set_config_dir(q(paths::config_dir()));
        self.as_mut().set_data_dir(q(paths::data_dir()));
        self.as_mut().set_config_file(q(paths::config_dir().join("parla.toml")));
        self.as_mut().set_dictionary_file(q(paths::dictionary()));
        self.as_mut().set_snippets_file(q(paths::snippets()));
        self.as_mut().set_apps_file(q(paths::apps()));
        self.as_mut().set_history_file(q(paths::history()));
        self.as_mut().set_autostart_file(q(autostart_path()));
        let on = autostart_path().exists();
        self.as_mut().rust_mut().autostart = on;
        self.as_mut().autostart_changed();
    }
}

impl qobject::Store {
    fn report(mut self: core::pin::Pin<&mut Self>, result: anyhow::Result<String>) -> QString {
        match result {
            Ok(s) => {
                self.as_mut().set_last_error(QString::from(""));
                QString::from(&s)
            }
            Err(e) => {
                self.as_mut().set_last_error(QString::from(&format!("{e:#}")));
                QString::from("")
            }
        }
    }

    fn report_ok(mut self: core::pin::Pin<&mut Self>, result: anyhow::Result<()>) -> bool {
        match result {
            Ok(()) => {
                self.as_mut().set_last_error(QString::from(""));
                true
            }
            Err(e) => {
                self.as_mut().set_last_error(QString::from(&format!("{e:#}")));
                false
            }
        }
    }

    pub fn load_dictionary(self: core::pin::Pin<&mut Self>) -> QString {
        self.report(Dictionary::load(&paths::dictionary()).and_then(|d| Ok(serde_json::to_string(&d)?)))
    }

    pub fn save_dictionary(self: core::pin::Pin<&mut Self>, json: &QString) -> bool {
        let r = serde_json::from_str::<Dictionary>(&json.to_string())
            .map_err(anyhow::Error::from)
            .and_then(|d| d.save(&paths::dictionary()));
        self.report_ok(r)
    }

    pub fn load_snippets(self: core::pin::Pin<&mut Self>) -> QString {
        self.report(Snippets::load(&paths::snippets()).and_then(|s| Ok(serde_json::to_string(&s)?)))
    }

    pub fn save_snippets(self: core::pin::Pin<&mut Self>, json: &QString) -> bool {
        let r = serde_json::from_str::<Snippets>(&json.to_string())
            .map_err(anyhow::Error::from)
            .and_then(|s| s.save(&paths::snippets()));
        self.report_ok(r)
    }

    pub fn load_apps(self: core::pin::Pin<&mut Self>) -> QString {
        self.report(AppProfiles::load(&paths::apps()).and_then(|a| Ok(serde_json::to_string(&a)?)))
    }

    pub fn save_apps(self: core::pin::Pin<&mut Self>, json: &QString) -> bool {
        let r = serde_json::from_str::<AppProfiles>(&json.to_string())
            .map_err(anyhow::Error::from)
            .and_then(|a| a.save(&paths::apps()));
        self.report_ok(r)
    }

    pub fn open_in_editor(self: core::pin::Pin<&mut Self>, path: &QString) -> bool {
        let path = PathBuf::from(path.to_string());
        let r = (|| -> anyhow::Result<()> {
            if !path.exists() {
                // A missing config is created empty so the editor has a file to open.
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, "")?;
            }
            std::process::Command::new("xdg-open")
                .arg(&path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| anyhow::anyhow!("running xdg-open: {e}"))?;
            Ok(())
        })();
        self.report_ok(r)
    }

    pub fn set_autostart(mut self: core::pin::Pin<&mut Self>, on: bool) {
        let path = autostart_path();
        let r = if on {
            paths::write_atomic(&path, AUTOSTART_ENTRY)
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(anyhow::anyhow!("removing {}: {e}", path.display())),
            }
        };
        let ok = self.as_mut().report_ok(r);
        let now = if ok { on } else { path.exists() };
        if self.rust().autostart != now {
            self.as_mut().rust_mut().autostart = now;
            self.as_mut().autostart_changed();
        }
    }
}
