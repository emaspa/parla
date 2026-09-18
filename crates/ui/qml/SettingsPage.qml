import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.parla.ui
import org.parla.host

FormCard.FormCardPage {
    id: page
    title: "Settings"

    // Paths as the daemon reports them; the Store's are the fallback.
    property var paths: ({})
    Component.onCompleted: if (Daemon.connected) Daemon.fetchPaths()
    Connections {
        target: Daemon
        function onPathsJsonChanged() {
            try { page.paths = JSON.parse(Daemon.pathsJson); } catch (e) { page.paths = {}; }
        }
        function onConnectedChanged() { if (Daemon.connected) Daemon.fetchPaths(); }
    }
    readonly property string configPath: paths.config || Store.configFile
    readonly property string dictionaryPath: paths.dictionary || Store.dictionaryFile
    readonly property string snippetsPath: paths.snippets || Store.snippetsFile
    readonly property string appsPath: paths.apps || Store.appsFile
    readonly property string historyPath: paths.history || Store.historyFile

    DaemonBanner {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.largeSpacing
    }

    FormCard.FormHeader { title: "Daemon" }
    FormCard.FormCard {
        FormCard.FormSwitchDelegate {
            text: "Enabled"
            description: "Off ignores the hotkeys until switched on again."
            checked: Daemon.enabled
            enabled: Daemon.connected
            onToggled: Daemon.enabled = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: "Status"
            description: applicationWindow().stateLabel
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: "Version"
            description: Daemon.connected ? (Daemon.version !== "" ? Daemon.version : "unknown") : "not running"
        }
    }

    FormCard.FormHeader { title: "Hotkeys and models" }
    FormCard.FormCard {
        FormCard.FormTextDelegate {
            text: "These are set in parla.toml and read by the daemon; edit the file and restart it."
            textItem.wrapMode: Text.WordWrap
            textItem.opacity: 0.7
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: "Dictation hotkey"
            description: Daemon.dictateHotkey !== "" ? Daemon.dictateHotkey : "–"
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: "Command hotkey"
            description: Daemon.commandHotkey !== "" ? Daemon.commandHotkey : "–"
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: "Cleanup model"
            description: Daemon.cleanup !== "" ? Daemon.cleanup : "off"
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: "Judge model"
            description: Daemon.judge !== "" ? Daemon.judge : "off"
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: "Open parla.toml in an editor"
            description: page.configPath
            icon.name: "document-edit"
            onClicked: if (!Store.openInEditor(page.configPath)) applicationWindow().showPassiveNotification(Store.lastError, "long")
        }
    }

    FormCard.FormHeader { title: "This app" }
    FormCard.FormCard {
        FormCard.FormSwitchDelegate {
            text: "Show a dot while idle"
            description: "A small mark at the bottom of the screen while parla waits for its hotkey."
            checked: applicationWindow().idleDot
            onToggled: applicationWindow().idleDot = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            text: "Start with the session"
            description: "Writes " + Store.autostartFile + " (the tray and overlay start, the window stays closed)."
            checked: Store.autostart
            onToggled: {
                Store.autostart = checked;
                if (Store.lastError !== "") applicationWindow().showPassiveNotification(Store.lastError, "long");
            }
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: "Tray"
            description: Host.trayBackend
        }
    }

    FormCard.FormHeader { title: "Files" }
    FormCard.FormCard {
        FormCard.FormButtonDelegate {
            text: "Dictionary"
            description: page.dictionaryPath
            icon.name: "accessories-dictionary"
            onClicked: Store.openInEditor(page.dictionaryPath)
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: "Snippets"
            description: page.snippetsPath
            icon.name: "edit-paste"
            onClicked: Store.openInEditor(page.snippetsPath)
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: "App profiles"
            description: page.appsPath
            icon.name: "preferences-desktop-apps"
            onClicked: Store.openInEditor(page.appsPath)
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: "History"
            description: page.historyPath
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: "Data directory"
            description: Store.dataDir
        }
    }
}
