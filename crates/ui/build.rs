//! Builds the Qt side of parla-ui: the cxx-qt bridges, the C++ host, and the
//! QML module embedded as a static plugin. Fails early with a readable message
//! when the Qt or KDE pieces it needs are not installed.

use std::path::{Path, PathBuf};
use std::process::Command;

use cxx_qt_build::{CxxQtBuilder, QmlModule};

const QML_FILES: &[&str] = &[
    "qml/Main.qml",
    "qml/Overlay.qml",
    "qml/HomePage.qml",
    "qml/HistoryPage.qml",
    "qml/DictionaryPage.qml",
    "qml/SnippetsPage.qml",
    "qml/AppsPage.qml",
    "qml/SettingsPage.qml",
    "qml/DaemonBanner.qml",
    "qml/WordsChart.qml",
    "qml/Format.qml",
    "qml/SelfTest.qml",
];

fn qmake_query(qmake: &str, var: &str) -> Option<String> {
    let out = Command::new(qmake).args(["-query", var]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn fail(msg: &str) -> ! {
    eprintln!("\nparla-ui cannot be built: {msg}\n");
    std::process::exit(1);
}

fn main() {
    // Locate Qt 6 through qmake6 (qt-build-utils does the same search; this
    // check exists to explain a missing piece instead of failing deep in it).
    let qmake = std::env::var("QMAKE").unwrap_or_else(|_| "qmake6".to_string());
    let version = qmake_query(&qmake, "QT_VERSION").unwrap_or_else(|| {
        fail(&format!(
            "`{qmake}` was not found or does not answer `-query`. Install Qt 6 \
             (qt6-base) or point QMAKE at your qmake6 binary."
        ))
    });
    if !version.starts_with("6.") {
        fail(&format!("Qt 6 is required, `{qmake}` reports Qt {version}"));
    }
    let qml_dir = PathBuf::from(
        qmake_query(&qmake, "QT_INSTALL_QML").unwrap_or_else(|| fail("qmake6 has no QT_INSTALL_QML")),
    );
    let headers = PathBuf::from(
        qmake_query(&qmake, "QT_INSTALL_HEADERS").unwrap_or_else(|| fail("qmake6 has no QT_INSTALL_HEADERS")),
    );
    let libs = PathBuf::from(
        qmake_query(&qmake, "QT_INSTALL_LIBS").unwrap_or_else(|| fail("qmake6 has no QT_INSTALL_LIBS")),
    );

    // Qt modules linked at build time.
    for (module, header) in [
        ("QtGui", "QGuiApplication"),
        ("QtWidgets", "QApplication"),
        ("QtQml", "QQmlApplicationEngine"),
        ("QtQuick", "QQuickWindow"),
        ("QtQuickControls2", "QQuickStyle"),
    ] {
        if !headers.join(module).join(header).exists() {
            fail(&format!(
                "Qt module {module} is missing (no {}/{module}/{header}). Install \
                 qt6-base and qt6-declarative with their headers.",
                headers.display()
            ));
        }
    }
    // QML modules used at runtime; checked here so a missing one is reported
    // at build time instead of as a blank window.
    for (module, path) in [
        ("Kirigami (kirigami)", "org/kde/kirigami"),
        ("Kirigami Addons (kirigami-addons)", "org/kde/kirigamiaddons/formcard"),
        ("qqc2-desktop-style", "org/kde/desktop"),
        ("layer-shell-qt", "org/kde/layershell"),
        ("QtQuick.Controls", "QtQuick/Controls"),
    ] {
        if !qml_dir.join(path).exists() {
            fail(&format!(
                "QML module {module} is missing (no {}/{path}).",
                qml_dir.display()
            ));
        }
    }

    // KStatusNotifierItem gives a proper Plasma tray item; without its headers
    // the tray falls back to QSystemTrayIcon (which Plasma also shows as SNI).
    let ksni_include = ["/usr/include/KF6/KStatusNotifierItem", "/usr/local/include/KF6/KStatusNotifierItem"]
        .into_iter()
        .map(Path::new)
        .find(|p| p.join("kstatusnotifieritem.h").exists())
        .map(Path::to_path_buf);
    let ksni_lib = libs.join("libKF6StatusNotifierItem.so").exists()
        || Path::new("/usr/lib/libKF6StatusNotifierItem.so").exists();
    let have_ksni = ksni_include.is_some() && ksni_lib;
    // KI18n provides i18n() to QML, which Kirigami Addons' form delegates call.
    let ki18n_include = ["/usr/include/KF6/KI18n", "/usr/local/include/KF6/KI18n"]
        .into_iter()
        .map(Path::new)
        .find(|p| p.join("klocalizedcontext.h").exists())
        .map(Path::to_path_buf);
    let have_ki18n = ki18n_include.is_some()
        && (libs.join("libKF6I18n.so").exists() || Path::new("/usr/lib/libKF6I18n.so").exists());
    if !have_ki18n {
        println!("cargo:warning=parla-ui: KF6 I18n not found, form delegates will log i18n warnings");
    }
    println!("cargo:warning=parla-ui: tray via {}", if have_ksni { "KStatusNotifierItem" } else { "QSystemTrayIcon" });

    let mut builder = CxxQtBuilder::new_qml_module(
        QmlModule::new("org.parla.ui")
            .qml_files(QML_FILES)
            .depend("QtQuick"),
    )
    .files(["src/host.rs", "src/daemon.rs", "src/store.rs"])
    .cpp_files(["cpp/host.h", "cpp/host.cpp"])
    .qt_module("Gui")
    .qt_module("Widgets")
    .qt_module("Qml")
    .qt_module("Quick")
    .qt_module("QuickControls2");

    // SAFETY: the callback only adds include paths and defines to the C++ build.
    builder = unsafe {
        builder.cc_builder(|cc| {
            cc.std("c++20");
            if let Some(inc) = &ksni_include {
                cc.include(inc);
                cc.define("PARLA_HAVE_KSNI", None);
            }
            if have_ki18n {
                if let Some(inc) = &ki18n_include {
                    cc.include(inc);
                }
                cc.define("PARLA_HAVE_KI18N", None);
            }
        })
    };
    builder.build();

    if have_ksni {
        println!("cargo:rustc-link-lib=KF6StatusNotifierItem");
    }
    if have_ki18n {
        println!("cargo:rustc-link-lib=KF6I18n");
    }
    println!("cargo:rerun-if-env-changed=QMAKE");
    for f in QML_FILES {
        println!("cargo:rerun-if-changed={f}");
    }
}
