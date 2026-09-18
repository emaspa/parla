// C++ glue for parla-ui: the QApplication (qqc2-desktop-style and the tray
// need QtWidgets), the QML engine, the tray item and a few desktop helpers
// QML cannot reach on its own. Everything that talks to the daemon or to
// the data files is in Rust; this file only hosts the window.
#pragma once

#include <QObject>
#include <QString>

#include "rust/cxx.h"

class QMenu;
class QAction;
#ifdef PARLA_HAVE_KSNI
class KStatusNotifierItem;
#else
class QSystemTrayIcon;
#endif

// The tray item, driven from QML: set the icon and tooltip, listen for the
// menu entries. Backed by KStatusNotifierItem when KF6 is available, else
// by QSystemTrayIcon (which Plasma renders through the same SNI protocol).
class Tray : public QObject {
    Q_OBJECT
    Q_PROPERTY(QString iconName READ iconName WRITE setIconName NOTIFY iconNameChanged)
    Q_PROPERTY(QString tooltipTitle READ tooltipTitle WRITE setTooltipTitle NOTIFY tooltipChanged)
    Q_PROPERTY(QString tooltipSubtitle READ tooltipSubtitle WRITE setTooltipSubtitle NOTIFY tooltipChanged)
    Q_PROPERTY(bool enabledChecked READ enabledChecked WRITE setEnabledChecked NOTIFY enabledCheckedChanged)
    Q_PROPERTY(bool attention READ attention WRITE setAttention NOTIFY attentionChanged)
public:
    explicit Tray(QObject *parent = nullptr);
    ~Tray() override;

    QString iconName() const { return m_iconName; }
    void setIconName(const QString &name);
    QString tooltipTitle() const { return m_tooltipTitle; }
    void setTooltipTitle(const QString &title);
    QString tooltipSubtitle() const { return m_tooltipSubtitle; }
    void setTooltipSubtitle(const QString &subtitle);
    bool enabledChecked() const { return m_enabledChecked; }
    void setEnabledChecked(bool on);
    bool attention() const { return m_attention; }
    void setAttention(bool on);

Q_SIGNALS:
    void iconNameChanged();
    void tooltipChanged();
    void enabledCheckedChanged();
    void attentionChanged();
    // Menu entries and clicks.
    void openRequested();
    void startDictationRequested();
    void enabledToggled(bool on);
    void quitRequested();

private:
    void applyTooltip();
    QString m_iconName = QStringLiteral("audio-input-microphone");
    QString m_tooltipTitle = QStringLiteral("parla");
    QString m_tooltipSubtitle;
    bool m_enabledChecked = true;
    bool m_attention = false;
    QMenu *m_menu = nullptr;
    QAction *m_enabledAction = nullptr;
#ifdef PARLA_HAVE_KSNI
    KStatusNotifierItem *m_item = nullptr;
#else
    QSystemTrayIcon *m_item = nullptr;
#endif
};

// Desktop helpers for QML.
class Host : public QObject {
    Q_OBJECT
    Q_PROPERTY(bool startHidden READ startHidden CONSTANT)
    Q_PROPERTY(QString trayBackend READ trayBackend CONSTANT)
    Q_PROPERTY(QString initialPage READ initialPage CONSTANT)
    Q_PROPERTY(QString shotPrefix READ shotPrefix CONSTANT)
    Q_PROPERTY(bool selfTest READ selfTest CONSTANT)
public:
    explicit Host(bool startHidden, QObject *parent = nullptr);
    bool startHidden() const { return m_startHidden; }
    // PARLA_UI_PAGE=history opens on that page; for screenshots and tests.
    QString initialPage() const;
    // PARLA_UI_SHOT=<prefix> makes the QML save grabs of its windows to
    // <prefix>-<name>.png; for screenshots and tests without a compositor grab.
    QString shotPrefix() const;
    // PARLA_UI_SELFTEST=1 runs the scripted walk-through in SelfTest.qml.
    bool selfTest() const;
    Q_INVOKABLE bool grabWindow(QObject *window, const QString &path);
    QString trayBackend() const;
    Q_INVOKABLE void copyText(const QString &text);
    Q_INVOKABLE bool openPath(const QString &path);
    Q_INVOKABLE void quit();

private:
    bool m_startHidden;
};

// Creates the application, registers the C++ types under org.parla.host,
// loads Main.qml and Overlay.qml from the embedded QML module and runs
// the event loop. Returns the exit code.
int run_ui(bool startHidden);
