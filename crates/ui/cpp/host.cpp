#include "parla-ui/cpp/host.h"

#include <QAction>
#include <QApplication>
#include <QClipboard>
#include <QDesktopServices>
#include <QIcon>
#include <QMenu>
#include <QQmlApplicationEngine>
#include <QQmlContext>
#include <QQuickWindow>
#include <QQuickStyle>
#include <QUrl>
#include <QtQml/qqml.h>

#ifdef PARLA_HAVE_KI18N
#include <KLocalizedContext>
#include <KLocalizedString>
#endif

#ifdef PARLA_HAVE_KSNI
#include <KStatusNotifierItem>
#else
#include <QSystemTrayIcon>
#endif

Tray::Tray(QObject *parent)
    : QObject(parent)
{
    m_menu = new QMenu();
    QAction *open = m_menu->addAction(QIcon::fromTheme(QStringLiteral("window")), tr("Open parla"));
    connect(open, &QAction::triggered, this, &Tray::openRequested);
    QAction *start = m_menu->addAction(QIcon::fromTheme(QStringLiteral("media-record")), tr("Start dictation"));
    connect(start, &QAction::triggered, this, &Tray::startDictationRequested);
    m_enabledAction = m_menu->addAction(tr("Enabled"));
    m_enabledAction->setCheckable(true);
    m_enabledAction->setChecked(m_enabledChecked);
    connect(m_enabledAction, &QAction::toggled, this, [this](bool on) {
        if (on != m_enabledChecked) {
            Q_EMIT enabledToggled(on);
        }
    });
    m_menu->addSeparator();
    QAction *quit = m_menu->addAction(QIcon::fromTheme(QStringLiteral("application-exit")), tr("Quit"));
    connect(quit, &QAction::triggered, this, &Tray::quitRequested);

#ifdef PARLA_HAVE_KSNI
    m_item = new KStatusNotifierItem(QStringLiteral("parla-ui"), this);
    m_item->setCategory(KStatusNotifierItem::ApplicationStatus);
    m_item->setTitle(QStringLiteral("parla"));
    m_item->setStandardActionsEnabled(false);
    m_item->setContextMenu(m_menu);
    m_item->setIconByName(m_iconName);
    m_item->setStatus(KStatusNotifierItem::Active);
    connect(m_item, &KStatusNotifierItem::activateRequested, this, [this](bool, const QPoint &) {
        Q_EMIT openRequested();
    });
    connect(m_item, &KStatusNotifierItem::secondaryActivateRequested, this, [this](const QPoint &) {
        Q_EMIT startDictationRequested();
    });
#else
    m_item = new QSystemTrayIcon(QIcon::fromTheme(m_iconName), this);
    m_item->setContextMenu(m_menu);
    connect(m_item, &QSystemTrayIcon::activated, this, [this](QSystemTrayIcon::ActivationReason reason) {
        if (reason == QSystemTrayIcon::Trigger || reason == QSystemTrayIcon::DoubleClick) {
            Q_EMIT openRequested();
        } else if (reason == QSystemTrayIcon::MiddleClick) {
            Q_EMIT startDictationRequested();
        }
    });
    m_item->show();
#endif
    applyTooltip();
}

Tray::~Tray()
{
    delete m_menu;
}

void Tray::setIconName(const QString &name)
{
    if (name == m_iconName) {
        return;
    }
    m_iconName = name;
#ifdef PARLA_HAVE_KSNI
    m_item->setIconByName(name);
#else
    m_item->setIcon(QIcon::fromTheme(name));
#endif
    Q_EMIT iconNameChanged();
}

void Tray::setTooltipTitle(const QString &title)
{
    if (title == m_tooltipTitle) {
        return;
    }
    m_tooltipTitle = title;
    applyTooltip();
    Q_EMIT tooltipChanged();
}

void Tray::setTooltipSubtitle(const QString &subtitle)
{
    if (subtitle == m_tooltipSubtitle) {
        return;
    }
    m_tooltipSubtitle = subtitle;
    applyTooltip();
    Q_EMIT tooltipChanged();
}

void Tray::applyTooltip()
{
#ifdef PARLA_HAVE_KSNI
    m_item->setToolTip(m_iconName, m_tooltipTitle, m_tooltipSubtitle);
#else
    m_item->setToolTip(m_tooltipSubtitle.isEmpty() ? m_tooltipTitle
                                                    : m_tooltipTitle + QStringLiteral("\n") + m_tooltipSubtitle);
#endif
}

void Tray::setEnabledChecked(bool on)
{
    if (on == m_enabledChecked) {
        return;
    }
    m_enabledChecked = on;
    m_enabledAction->setChecked(on);
    Q_EMIT enabledCheckedChanged();
}

void Tray::setAttention(bool on)
{
    if (on == m_attention) {
        return;
    }
    m_attention = on;
#ifdef PARLA_HAVE_KSNI
    m_item->setStatus(on ? KStatusNotifierItem::NeedsAttention : KStatusNotifierItem::Active);
#endif
    Q_EMIT attentionChanged();
}

Host::Host(bool startHidden, QObject *parent)
    : QObject(parent)
    , m_startHidden(startHidden)
{
}

QString Host::trayBackend() const
{
#ifdef PARLA_HAVE_KSNI
    return QStringLiteral("KStatusNotifierItem");
#else
    return QStringLiteral("QSystemTrayIcon");
#endif
}

QString Host::initialPage() const
{
    return qEnvironmentVariable("PARLA_UI_PAGE");
}

QString Host::shotPrefix() const
{
    return qEnvironmentVariable("PARLA_UI_SHOT");
}

bool Host::selfTest() const
{
    return qEnvironmentVariableIsSet("PARLA_UI_SELFTEST");
}

bool Host::grabWindow(QObject *window, const QString &path)
{
    auto *quick = qobject_cast<QQuickWindow *>(window);
    if (!quick || !quick->isVisible()) {
        return false;
    }
    return quick->grabWindow().save(path);
}

void Host::copyText(const QString &text)
{
    QGuiApplication::clipboard()->setText(text);
}

bool Host::openPath(const QString &path)
{
    return QDesktopServices::openUrl(QUrl::fromLocalFile(path));
}

void Host::quit()
{
    QCoreApplication::quit();
}

int run_ui(bool startHidden)
{
    QCoreApplication::setOrganizationName(QStringLiteral("parla"));
    QCoreApplication::setOrganizationDomain(QStringLiteral("parla.local"));
    QCoreApplication::setApplicationName(QStringLiteral("parla-ui"));
    QGuiApplication::setDesktopFileName(QStringLiteral("parla-ui"));
    if (qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE")) {
        // Breeze widgets look for QtQuick Controls, as every Plasma app does.
        QQuickStyle::setStyle(QStringLiteral("org.kde.desktop"));
    }

    static int argc = 1;
    static char name[] = "parla-ui";
    static char *argv[] = {name, nullptr};
    QApplication app(argc, argv);
    app.setQuitOnLastWindowClosed(false);
    QApplication::setWindowIcon(QIcon::fromTheme(QStringLiteral("audio-input-microphone")));

    qmlRegisterType<Tray>("org.parla.host", 1, 0, "Tray");
    qmlRegisterSingletonInstance("org.parla.host", 1, 0, "Host", new Host(startHidden, &app));

    QQmlApplicationEngine engine;
#ifdef PARLA_HAVE_KI18N
    // Kirigami Addons' form delegates call i18n(); this provides it.
    KLocalizedString::setApplicationDomain(QByteArrayLiteral("parla-ui"));
    engine.rootContext()->setContextObject(new KLocalizedContext(&engine));
#endif
    engine.load(QUrl(QStringLiteral("qrc:/qt/qml/org/parla/ui/qml/Main.qml")));
    if (engine.rootObjects().isEmpty()) {
        return 1;
    }
    engine.load(QUrl(QStringLiteral("qrc:/qt/qml/org/parla/ui/qml/Overlay.qml")));
    return app.exec();
}
