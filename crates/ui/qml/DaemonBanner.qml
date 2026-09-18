import QtQuick
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.parla.ui

// The banner at the top of every page: the daemon is missing, or it sent
// an error. The message lives on the window so it survives page changes.
Kirigami.InlineMessage {
    id: banner
    Layout.fillWidth: true
    readonly property bool offline: !Daemon.connected
    readonly property string message: applicationWindow().lastError
    visible: offline || message !== ""
    type: offline ? Kirigami.MessageType.Warning : Kirigami.MessageType.Error
    icon.source: offline ? "dialog-warning" : "dialog-error"
    text: offline
        ? "parlad is not running. Start it (run `parlad`, or log in again if it autostarts); this window reconnects on its own."
        : message
    actions: [
        Kirigami.Action {
            text: "Dismiss"
            icon.name: "dialog-close"
            visible: !banner.offline
            onTriggered: applicationWindow().lastError = ""
        }
    ]
}
