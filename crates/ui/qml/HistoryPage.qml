import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.parla.ui
import org.parla.host

Kirigami.ScrollablePage {
    id: page
    title: "History"

    property var records: []
    property string query: ""
    readonly property int pageSize: 50
    property bool exhausted: false
    property bool loading: false
    property bool confirmClear: false

    Format { id: fmt }

    readonly property var filtered: {
        const q = query.trim().toLowerCase();
        if (q === "") return records;
        return records.filter(r => r.text.toLowerCase().includes(q) || r.raw.toLowerCase().includes(q)
                                || r.app.toLowerCase().includes(q) || r.title.toLowerCase().includes(q));
    }

    function reload() {
        records = [];
        exhausted = false;
        loadMore();
    }
    function loadMore() {
        if (!Daemon.connected || loading || exhausted) return;
        loading = true;
        Daemon.fetchHistory(pageSize, records.length);
    }
    Component.onCompleted: reload()

    Connections {
        target: Daemon
        function onHistoryLoaded(json, offset) {
            page.loading = false;
            let batch = [];
            try { batch = JSON.parse(json); } catch (e) { console.warn("history:", e); }
            if (offset === 0) {
                page.records = batch;
            } else if (offset === page.records.length) {
                const seen = new Set(page.records.map(r => r.id));
                page.records = page.records.concat(batch.filter(r => !seen.has(r.id)));
            }
            page.exhausted = batch.length < page.pageSize;
        }
        function onUtterance(json) {
            try { page.records = [JSON.parse(json)].concat(page.records); } catch (e) { console.warn("utterance:", e); }
        }
        function onHistoryDeleted(id) { page.records = page.records.filter(r => r.id !== id); }
        function onHistoryCleared() { page.records = []; page.exhausted = true; }
        function onConnectedChanged() { if (Daemon.connected) page.reload(); }
    }

    actions: [
        Kirigami.Action {
            text: "Refresh"
            icon.name: "view-refresh"
            onTriggered: page.reload()
        },
        Kirigami.Action {
            text: "Clear all"
            icon.name: "edit-clear-history"
            enabled: Daemon.connected && page.records.length > 0
            onTriggered: page.confirmClear = true
        }
    ]

    header: ColumnLayout {
        spacing: Kirigami.Units.smallSpacing
        DaemonBanner {
            Layout.margins: Kirigami.Units.largeSpacing
            Layout.bottomMargin: 0
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.largeSpacing
            Layout.bottomMargin: 0
            visible: page.confirmClear
            type: Kirigami.MessageType.Warning
            text: "Delete all " + page.records.length + " records? This cannot be undone."
            actions: [
                Kirigami.Action {
                    text: "Delete everything"
                    icon.name: "edit-delete"
                    onTriggered: { Daemon.clearHistory(); page.confirmClear = false; }
                },
                Kirigami.Action {
                    text: "Keep"
                    icon.name: "dialog-cancel"
                    onTriggered: page.confirmClear = false
                }
            ]
        }
        Kirigami.SearchField {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.largeSpacing
            placeholderText: "Search text, app or window title"
            onTextChanged: page.query = text
        }
    }

    ListView {
        id: list
        model: page.filtered
        spacing: 0
        clip: true
        currentIndex: -1

        Kirigami.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 4
            visible: list.count === 0 && !page.loading
            icon.name: "view-history"
            text: !Daemon.connected ? "History needs the daemon" : page.query !== "" ? "Nothing matches" : "No dictation yet"
        }

        delegate: QQC2.ItemDelegate {
            id: row
            required property var modelData
            required property int index
            width: ListView.view.width
            property bool confirming: false
            hoverEnabled: true
            background: Rectangle {
                color: row.hovered ? Kirigami.Theme.alternateBackgroundColor : "transparent"
                Kirigami.Separator { anchors.left: parent.left; anchors.right: parent.right; anchors.bottom: parent.bottom }
            }
            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing
                Kirigami.Icon {
                    source: fmt.outcomeIcon(row.modelData.outcome)
                    Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                    Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                    Layout.alignment: Qt.AlignTop
                    Layout.topMargin: Kirigami.Units.smallSpacing
                    opacity: 0.8
                    color: row.modelData.outcome === "error" ? Kirigami.Theme.negativeTextColor : Kirigami.Theme.textColor
                }
                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing / 2
                    QQC2.Label {
                        text: row.modelData.text
                        wrapMode: Text.WordWrap
                        maximumLineCount: 3
                        elide: Text.ElideRight
                        Layout.fillWidth: true
                    }
                    QQC2.Label {
                        text: fmt.appName(row.modelData.app)
                              + (row.modelData.title !== "" ? " (" + row.modelData.title + ")" : "")
                              + "  ·  " + fmt.when(row.modelData.at_ms)
                              + "  ·  " + fmt.outcomeLabel(row.modelData)
                              + (row.modelData.words > 0 ? "  ·  " + row.modelData.words + " words" : "")
                              + (row.modelData.profile !== "" ? "  ·  " + row.modelData.profile : "")
                        opacity: 0.7
                        font: Kirigami.Theme.smallFont
                        elide: Text.ElideRight
                        Layout.fillWidth: true
                    }
                }
                // Actions, or the inline delete confirmation.
                RowLayout {
                    spacing: 0
                    Layout.alignment: Qt.AlignVCenter
                    visible: !row.confirming
                    QQC2.ToolButton {
                        icon.name: "edit-copy"
                        display: QQC2.AbstractButton.IconOnly
                        text: "Copy text"
                        QQC2.ToolTip.text: text
                        QQC2.ToolTip.visible: hovered
                        onClicked: { Host.copyText(row.modelData.text); applicationWindow().showPassiveNotification("Copied"); }
                    }
                    QQC2.ToolButton {
                        icon.name: "audio-input-microphone"
                        display: QQC2.AbstractButton.IconOnly
                        text: "Copy raw transcript"
                        enabled: row.modelData.raw !== ""
                        QQC2.ToolTip.text: text + (row.modelData.raw !== "" ? "\n“" + row.modelData.raw + "”" : "")
                        QQC2.ToolTip.visible: hovered
                        onClicked: { Host.copyText(row.modelData.raw); applicationWindow().showPassiveNotification("Copied the raw transcript"); }
                    }
                    QQC2.ToolButton {
                        icon.name: "edit-delete"
                        display: QQC2.AbstractButton.IconOnly
                        text: "Delete"
                        QQC2.ToolTip.text: text
                        QQC2.ToolTip.visible: hovered
                        onClicked: row.confirming = true
                    }
                }
                RowLayout {
                    visible: row.confirming
                    Layout.alignment: Qt.AlignVCenter
                    QQC2.Label { text: "Delete?" }
                    QQC2.Button {
                        text: "Delete"
                        icon.name: "edit-delete"
                        onClicked: { Daemon.deleteHistory(row.modelData.id); row.confirming = false; }
                    }
                    QQC2.Button {
                        text: "Keep"
                        onClicked: row.confirming = false
                    }
                }
            }
        }

        footer: Item {
            width: ListView.view ? ListView.view.width : 0
            height: visible ? Kirigami.Units.gridUnit * 3 : 0
            visible: !page.exhausted && page.records.length > 0
            QQC2.Button {
                anchors.centerIn: parent
                text: page.loading ? "Loading…" : "Load older"
                enabled: !page.loading
                onClicked: page.loadMore()
            }
        }
        onAtYEndChanged: if (atYEnd && contentHeight > height) page.loadMore()
    }
}
