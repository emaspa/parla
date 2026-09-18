import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.parla.ui

Kirigami.ScrollablePage {
    id: page
    title: "Home"

    property var stats: ({ utterances: 0, words: 0, audio_ms: 0, words_per_minute: 0, days: [] })
    property var recent: []
    readonly property int todayWords: stats.days.length > 0 ? stats.days[stats.days.length - 1].words : 0
    readonly property int todayUtterances: stats.days.length > 0 ? stats.days[stats.days.length - 1].utterances : 0

    Format { id: fmt }

    function refresh() {
        if (Daemon.connected) {
            Daemon.fetchStats();
            Daemon.fetchHistory(5, 0);
        }
    }
    Component.onCompleted: refresh()

    Connections {
        target: Daemon
        function onStatsJsonChanged() {
            if (Daemon.statsJson !== "") {
                try { page.stats = JSON.parse(Daemon.statsJson); } catch (e) { console.warn("stats:", e); }
            }
        }
        function onHistoryLoaded(json, offset) {
            if (offset === 0) {
                try { page.recent = JSON.parse(json).slice(0, 5); } catch (e) { console.warn("history:", e); }
            }
        }
        function onUtterance(json) {
            try {
                const r = JSON.parse(json);
                page.recent = [r].concat(page.recent).slice(0, 5);
            } catch (e) { console.warn("utterance:", e); }
            Daemon.fetchStats();
        }
        function onHistoryCleared() { page.recent = []; page.refresh(); }
        function onHistoryDeleted(id) { page.recent = page.recent.filter(r => r.id !== id); }
        function onConnectedChanged() { page.refresh(); }
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        DaemonBanner {}

        // Status card: what the daemon is doing, and the record button.
        Kirigami.AbstractCard {
            Layout.fillWidth: true
            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing * 2

                QQC2.RoundButton {
                    id: recordButton
                    readonly property bool recording: Daemon.state === "recording"
                    readonly property bool canStart: Daemon.connected && Daemon.enabled && Daemon.state === "idle"
                    enabled: recording || canStart
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 5
                    Layout.preferredHeight: Kirigami.Units.gridUnit * 5
                    Layout.alignment: Qt.AlignVCenter
                    icon.name: recording ? "media-playback-stop" : "audio-input-microphone"
                    icon.width: Kirigami.Units.iconSizes.large
                    icon.height: Kirigami.Units.iconSizes.large
                    icon.color: recording ? Kirigami.Theme.negativeTextColor : undefined
                    QQC2.ToolTip.text: recording ? "Stop and type what was said" : "Start hands-free dictation"
                    QQC2.ToolTip.visible: hovered
                    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                    onClicked: recording ? Daemon.stop() : Daemon.start("dictate")
                }

                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing
                    Kirigami.Heading {
                        level: 2
                        text: applicationWindow().stateLabel
                        Layout.fillWidth: true
                        elide: Text.ElideRight
                    }
                    QQC2.Label {
                        visible: Daemon.connected
                        text: Daemon.dictateHotkey !== "" || Daemon.commandHotkey !== ""
                            ? "Hold " + (Daemon.dictateHotkey || "?") + " to dictate, " + (Daemon.commandHotkey || "?") + " for a command"
                            : "No hotkeys configured"
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    QQC2.Label {
                        visible: Daemon.connected
                        text: "Cleanup: " + (Daemon.cleanup !== "" ? Daemon.cleanup : "off") + "   ·   Judge: " + (Daemon.judge !== "" ? Daemon.judge : "off")
                        opacity: 0.7
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    QQC2.Label {
                        visible: Daemon.connected && Daemon.version !== ""
                        text: "parlad " + Daemon.version
                        opacity: 0.7
                    }
                }
            }
        }

        // Numbers.
        GridLayout {
            Layout.fillWidth: true
            columns: page.width > Kirigami.Units.gridUnit * 40 ? 4 : 2
            columnSpacing: Kirigami.Units.largeSpacing
            rowSpacing: Kirigami.Units.largeSpacing
            Repeater {
                model: [
                    { label: "Words today", value: fmt.number(page.todayWords), sub: page.todayUtterances + (page.todayUtterances === 1 ? " utterance" : " utterances") },
                    { label: "Words in total", value: fmt.number(page.stats.words), sub: fmt.number(page.stats.utterances) + " utterances" },
                    { label: "Words per minute", value: page.stats.words_per_minute > 0 ? Math.round(page.stats.words_per_minute) : "–", sub: "while speaking" },
                    { label: "Time spoken", value: page.stats.audio_ms >= 3_600_000 ? (page.stats.audio_ms / 3_600_000).toFixed(1) + " h" : Math.round(page.stats.audio_ms / 60_000) + " min", sub: "in total" }
                ]
                Kirigami.AbstractCard {
                    required property var modelData
                    Layout.fillWidth: true
                    contentItem: ColumnLayout {
                        spacing: 0
                        QQC2.Label { text: modelData.label; opacity: 0.7; font: Kirigami.Theme.smallFont }
                        Kirigami.Heading { level: 1; text: modelData.value }
                        QQC2.Label { text: modelData.sub; opacity: 0.7; font: Kirigami.Theme.smallFont }
                    }
                }
            }
        }

        // Chart.
        Kirigami.AbstractCard {
            Layout.fillWidth: true
            header: Kirigami.Heading { level: 3; text: "Words per day, last 30 days" }
            contentItem: WordsChart {
                days: page.stats.days
            }
        }

        // Recent utterances.
        Kirigami.AbstractCard {
            Layout.fillWidth: true
            header: RowLayout {
                Kirigami.Heading { level: 3; text: "Recent"; Layout.fillWidth: true }
                QQC2.ToolButton {
                    text: "All history"
                    icon.name: "view-history"
                    onClicked: applicationWindow().navigate("history")
                }
            }
            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                Kirigami.PlaceholderMessage {
                    visible: page.recent.length === 0
                    Layout.fillWidth: true
                    Layout.topMargin: Kirigami.Units.largeSpacing
                    Layout.bottomMargin: Kirigami.Units.largeSpacing
                    icon.name: "audio-input-microphone"
                    text: Daemon.connected ? "Nothing dictated yet" : "History needs the daemon"
                    explanation: Daemon.connected ? "Hold the dictation hotkey and speak; what you say shows up here." : ""
                }
                Repeater {
                    model: page.recent
                    ColumnLayout {
                        required property var modelData
                        required property int index
                        Layout.fillWidth: true
                        spacing: 0
                        Kirigami.Separator { visible: index > 0; Layout.fillWidth: true; Layout.bottomMargin: Kirigami.Units.smallSpacing }
                        QQC2.Label {
                            text: modelData.text
                            wrapMode: Text.WordWrap
                            maximumLineCount: 2
                            elide: Text.ElideRight
                            Layout.fillWidth: true
                        }
                        RowLayout {
                            spacing: Kirigami.Units.smallSpacing
                            Kirigami.Icon {
                                source: fmt.outcomeIcon(modelData.outcome)
                                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                opacity: 0.7
                            }
                            QQC2.Label {
                                text: fmt.appName(modelData.app) + "  ·  " + fmt.timeAgo(modelData.at_ms) + "  ·  " + fmt.outcomeLabel(modelData)
                                opacity: 0.7
                                font: Kirigami.Theme.smallFont
                                elide: Text.ElideRight
                                Layout.fillWidth: true
                            }
                        }
                    }
                }
            }
        }
    }
}
