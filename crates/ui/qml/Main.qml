import QtQuick
import QtQuick.Controls as QQC2
import QtCore
import org.kde.kirigami as Kirigami
import org.parla.ui
import org.parla.host

// The main window: a sidebar of pages, the tray item and the overlay. It
// hides instead of closing so the tray and overlay keep running.
Kirigami.ApplicationWindow {
    id: root
    title: "parla"
    width: 980
    height: 700
    minimumWidth: 720
    minimumHeight: 480
    visible: !Host.startHidden

    // The last message the daemon sent; pages show it in their banner.
    property string lastError: ""
    property string currentPage: ""
    property alias idleDot: uiSettings.idleDot

    Settings {
        id: uiSettings
        category: "overlay"
        // Show a small dot at the bottom of the screen while idle, as a
        // reminder that parla is listening for its hotkey.
        property bool idleDot: false
    }

    readonly property var pages: ({
        "home": homePage, "history": historyPage, "dictionary": dictionaryPage,
        "snippets": snippetsPage, "apps": appsPage, "settings": settingsPage
    })
    Component { id: homePage; HomePage {} }
    Component { id: historyPage; HistoryPage {} }
    Component { id: dictionaryPage; DictionaryPage {} }
    Component { id: snippetsPage; SnippetsPage {} }
    Component { id: appsPage; AppsPage {} }
    Component { id: settingsPage; SettingsPage {} }

    function navigate(name) {
        if (currentPage === name) {
            return;
        }
        currentPage = name;
        // Pages are created here with a parent and destroyed on leaving, so
        // the row never owns a parentless item (which Qt warns about).
        const old = pageStack.items.slice();
        pageStack.clear();
        old.forEach(p => p.destroy());
        pageStack.push(pages[name].createObject(pageStack));
    }

    function present() {
        root.show();
        root.raise();
        root.requestActivate();
    }

    Component.onCompleted: navigate(pages[Host.initialPage] ? Host.initialPage : "home")

    globalDrawer: Kirigami.GlobalDrawer {
        modal: false
        collapsible: false
        width: Kirigami.Units.gridUnit * 11
        actions: [
            Kirigami.Action {
                text: "Home"
                icon.name: "go-home"
                checked: root.currentPage === "home"
                onTriggered: root.navigate("home")
            },
            Kirigami.Action {
                text: "History"
                icon.name: "view-history"
                checked: root.currentPage === "history"
                onTriggered: root.navigate("history")
            },
            Kirigami.Action {
                text: "Dictionary"
                icon.name: "accessories-dictionary"
                checked: root.currentPage === "dictionary"
                onTriggered: root.navigate("dictionary")
            },
            Kirigami.Action {
                text: "Snippets"
                icon.name: "edit-paste"
                checked: root.currentPage === "snippets"
                onTriggered: root.navigate("snippets")
            },
            Kirigami.Action {
                text: "Apps"
                icon.name: "preferences-desktop-apps"
                checked: root.currentPage === "apps"
                onTriggered: root.navigate("apps")
            },
            Kirigami.Action {
                text: "Settings"
                icon.name: "configure"
                checked: root.currentPage === "settings"
                onTriggered: root.navigate("settings")
            }
        ]
    }

    onClosing: (close) => {
        // Closing the window keeps parla in the tray; Quit is in its menu.
        close.accepted = false;
        root.hide();
    }

    Connections {
        target: Daemon
        function onError(message) {
            root.lastError = message;
            root.showPassiveNotification(message, "long");
        }
        function onReloaded(ok, message) {
            if (!ok) {
                root.lastError = "Reload failed: " + message;
                root.showPassiveNotification(root.lastError, "long");
            }
        }
        function onConnectedChanged() {
            if (Daemon.connected) {
                root.lastError = "";
            }
        }
    }

    readonly property string stateLabel: {
        if (!Daemon.connected) return "parlad is not running";
        switch (Daemon.state) {
        case "idle": return Daemon.enabled ? "Ready" : "Disabled";
        case "recording": return Daemon.mode === "command" ? "Listening for a command" : "Listening";
        case "transcribing": return "Transcribing";
        case "thinking": return "Thinking";
        case "typing": return "Typing";
        case "waiting": return "Waiting for confirmation";
        case "paused": return Daemon.enabled ? "Paused (session locked)" : "Disabled";
        default: return Daemon.state;
        }
    }

    Tray {
        id: tray
        iconName: {
            if (!Daemon.connected) return "dialog-warning";
            if (!Daemon.enabled || Daemon.state === "paused") return "microphone-sensitivity-muted";
            if (Daemon.state === "recording") return "media-record";
            if (Daemon.state === "idle") return "audio-input-microphone";
            return "microphone-sensitivity-high";
        }
        tooltipTitle: "parla"
        tooltipSubtitle: root.stateLabel
        enabledChecked: Daemon.enabled
        attention: !Daemon.connected
        onOpenRequested: root.visible && root.active ? root.hide() : root.present()
        onStartDictationRequested: Daemon.state === "recording" ? Daemon.stop() : Daemon.start("dictate")
        onEnabledToggled: (on) => Daemon.enabled = on
        onQuitRequested: Host.quit()
    }

    Overlay {
        id: overlay
        idleDot: uiSettings.idleDot
        onPhaseChanged: if (Host.shotPrefix !== "" && phase !== "hidden") overlayGrab.restart()
    }

    // Test hooks: PARLA_UI_SELFTEST walks through the pages as clicks would;
    // PARLA_UI_SHOT saves the window and each overlay phase.
    SelfTest {
        running: Host.selfTest
    }
    Timer {
        running: Host.shotPrefix !== ""
        interval: 3500
        onTriggered: console.info("grab", root.currentPage, Host.grabWindow(root, Host.shotPrefix + "-" + root.currentPage + ".png"))
    }
    Timer {
        id: overlayGrab
        interval: 450
        onTriggered: Host.grabWindow(overlay, Host.shotPrefix + "-overlay-" + overlay.phase + ".png")
    }
}
