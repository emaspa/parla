import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.layershell as LayerShell
import org.parla.ui

// The pill at the bottom of the screen. A layer-shell overlay so it floats
// above everything without taking focus from the window being dictated
// into. Hidden while idle unless the idle dot is on.
Window {
    id: overlay
    property bool idleDot: false

    // Not a transient of the main window: it must stay when that is hidden
    // and layer-shell only accepts toplevels.
    transientParent: null
    flags: Qt.FramelessWindowHint | Qt.WindowDoesNotAcceptFocus | Qt.WindowStaysOnTopHint
    color: "transparent"
    title: "parla overlay"

    LayerShell.Window.layer: LayerShell.Window.LayerOverlay
    LayerShell.Window.anchors: LayerShell.Window.AnchorBottom
    LayerShell.Window.exclusionZone: 0
    LayerShell.Window.keyboardInteractivity: LayerShell.Window.KeyboardInteractivityNone
    LayerShell.Window.scope: "parla-overlay"

    readonly property string daemonState: Daemon.state
    readonly property bool recording: daemonState === "recording"
    readonly property bool busy: daemonState === "transcribing" || daemonState === "thinking" || daemonState === "typing"
    property bool flash: false
    readonly property string phase: {
        if (!Daemon.connected) return "hidden";
        if (recording) return "recording";
        if (busy) return "busy";
        if (daemonState === "waiting") return "waiting";
        if (flash) return "flash";
        if (daemonState === "idle" && Daemon.enabled && idleDot) return "dot";
        return "hidden";
    }
    visible: phase !== "hidden"

    readonly property int bottomMargin: Kirigami.Units.gridUnit * 2
    readonly property int pad: Kirigami.Units.largeSpacing
    width: Math.max(pill.implicitWidth, dot.width) + 2 * pad
    height: (phase === "dot" ? dot.height : pill.implicitHeight) + pad + bottomMargin

    // The result flash: shown for a moment after a capture finishes.
    Connections {
        target: Daemon
        function onLastResultChanged() {
            if (Daemon.lastResult !== "" && (Daemon.state === "idle" || Daemon.state === "paused")) {
                overlay.flash = true;
                flashTimer.restart();
            }
        }
        function onStateChanged() {
            if (Daemon.state !== "idle" && Daemon.state !== "paused") {
                overlay.flash = false;
                flashTimer.stop();
            }
        }
    }
    Timer {
        id: flashTimer
        interval: 2200
        onTriggered: overlay.flash = false
    }

    // Smoothed microphone level.
    property real level: Daemon.level
    Behavior on level {
        NumberAnimation { duration: 70 }
    }

    Kirigami.Theme.inherit: false
    Kirigami.Theme.colorSet: Kirigami.Theme.Window

    // Idle dot.
    Rectangle {
        id: dot
        visible: overlay.phase === "dot"
        width: Kirigami.Units.gridUnit * 0.6
        height: width
        radius: width / 2
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.bottom: parent.bottom
        anchors.bottomMargin: overlay.bottomMargin
        color: dotArea.containsMouse ? Kirigami.Theme.highlightColor : Kirigami.Theme.disabledTextColor
        opacity: 0.8
        MouseArea {
            id: dotArea
            anchors.fill: parent
            anchors.margins: -Kirigami.Units.largeSpacing
            hoverEnabled: true
            cursorShape: Qt.PointingHandCursor
            onClicked: Daemon.start("dictate")
        }
    }

    Kirigami.ShadowedRectangle {
        id: pill
        visible: overlay.phase !== "dot"
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.bottom: parent.bottom
        anchors.bottomMargin: overlay.bottomMargin
        implicitWidth: row.implicitWidth + 2 * Kirigami.Units.largeSpacing * 1.5
        implicitHeight: Kirigami.Units.gridUnit * 2.6
        width: implicitWidth
        height: implicitHeight
        radius: height / 2
        color: Kirigami.Theme.backgroundColor
        border.width: 1
        border.color: Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.2)
        shadow.size: Kirigami.Units.largeSpacing * 2
        shadow.color: Qt.rgba(0, 0, 0, 0.35)
        shadow.yOffset: 2

        readonly property color accent: {
            if (overlay.phase === "flash") return Daemon.lastResultIsError ? Kirigami.Theme.negativeTextColor : Kirigami.Theme.positiveTextColor;
            if (overlay.phase === "recording") return Daemon.mode === "command" ? Kirigami.Theme.highlightColor : Kirigami.Theme.negativeTextColor;
            return Kirigami.Theme.highlightColor;
        }

        RowLayout {
            id: row
            anchors.centerIn: parent
            spacing: Kirigami.Units.largeSpacing

            // Recording: a mic and live level bars.
            Kirigami.Icon {
                visible: overlay.phase === "recording"
                source: "audio-input-microphone-symbolic"
                color: pill.accent
                Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            }
            Row {
                visible: overlay.phase === "recording"
                spacing: 3
                Layout.alignment: Qt.AlignVCenter
                Repeater {
                    model: [0.55, 0.85, 1.0, 0.75, 0.5]
                    Rectangle {
                        required property real modelData
                        width: 4
                        radius: 2
                        anchors.verticalCenter: parent.verticalCenter
                        color: pill.accent
                        height: 4 + Math.min(1, overlay.level * 1.6) * modelData * (Kirigami.Units.gridUnit * 1.2)
                    }
                }
            }

            // Busy: spinner.
            QQC2.BusyIndicator {
                visible: overlay.phase === "busy"
                running: visible
                Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            }

            // Waiting and flash: a status icon.
            Kirigami.Icon {
                visible: overlay.phase === "waiting" || overlay.phase === "flash"
                source: overlay.phase === "waiting" ? "dialog-question" : (Daemon.lastResultIsError ? "dialog-error" : "dialog-ok-apply")
                color: pill.accent
                Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            }

            QQC2.Label {
                text: {
                    switch (overlay.phase) {
                    case "recording": return Daemon.mode === "command" ? "Command" : "Listening";
                    case "busy": return Daemon.state === "transcribing" ? "Transcribing" : Daemon.state === "thinking" ? "Thinking" : "Typing";
                    case "waiting": return Daemon.confirmQuestion !== "" ? Daemon.confirmQuestion : "Say yes or no";
                    case "flash": return Daemon.lastResult;
                    default: return "";
                    }
                }
                font.bold: overlay.phase === "recording"
                elide: Text.ElideRight
                Layout.maximumWidth: Kirigami.Units.gridUnit * 28
            }
        }

        MouseArea {
            anchors.fill: parent
            acceptedButtons: Qt.LeftButton | Qt.RightButton
            cursorShape: overlay.phase === "recording" || overlay.phase === "flash" ? Qt.PointingHandCursor : Qt.ArrowCursor
            onClicked: (mouse) => {
                if (overlay.phase === "recording") {
                    if (mouse.button === Qt.RightButton) {
                        Daemon.cancel();
                    } else {
                        Daemon.stop();
                    }
                } else if (overlay.phase === "flash" && mouse.button === Qt.LeftButton) {
                    // The flash doubles as a start button, like the idle dot.
                    overlay.flash = false;
                    Daemon.start("dictate");
                }
            }
        }
    }
}
