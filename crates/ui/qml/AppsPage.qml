import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.parla.ui

// Per-application profiles: how dictated text should read where it lands.
FormCard.FormCardPage {
    id: appsPage
    title: "Apps"

    property var profiles: []
    property string fileError: ""
    readonly property var tones: ["neutral", "casual", "formal", "code"]
    readonly property var toneLabels: ["Neutral: the speaker's own register", "Casual: chat messages", "Formal: mail and documents", "Code: terminals and editors"]
    property string previewResult: ""
    property bool previewing: false

    function load() {
        const json = Store.loadApps();
        if (json === "") {
            fileError = Store.lastError;
            return;
        }
        fileError = "";
        profiles = JSON.parse(json).app || [];
    }
    function save() {
        const ok = Store.saveApps(JSON.stringify({ app: profiles }));
        fileError = ok ? "" : Store.lastError;
        if (ok) {
            Daemon.reload();
        }
    }
    function add() {
        profiles = profiles.concat([{ name: "New profile", class: [], tone: "neutral", cleanup: true, instructions: "" }]);
        save();
    }
    function remove(i) {
        profiles = profiles.filter((_, k) => k !== i);
        save();
    }
    function move(i, delta) {
        const j = i + delta;
        if (j < 0 || j >= profiles.length) return;
        const copy = profiles.slice();
        const tmp = copy[i];
        copy[i] = copy[j];
        copy[j] = tmp;
        profiles = copy;
        save();
    }
    // Field edits change the object in place so the cards are not rebuilt.
    function set(i, key, value) {
        if (JSON.stringify(profiles[i][key]) === JSON.stringify(value)) return;
        profiles[i][key] = value;
        save();
    }
    function parseClasses(text) {
        return text.split(/[,\s]+/).map(s => s.trim()).filter(s => s !== "");
    }
    Component.onCompleted: load()

    Connections {
        target: Daemon
        function onPreviewReady(cleaned) {
            appsPage.previewing = false;
            appsPage.previewResult = cleaned;
        }
        function onError() { appsPage.previewing = false; }
    }

    actions: [
        Kirigami.Action {
            text: "Add profile"
            icon.name: "list-add"
            onTriggered: appsPage.add()
        },
        Kirigami.Action {
            text: "Open file"
            icon.name: "document-edit"
            onTriggered: Store.openInEditor(Store.appsFile)
        }
    ]


    DaemonBanner {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.largeSpacing
    }
    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.largeSpacing
        visible: appsPage.fileError !== ""
        type: Kirigami.MessageType.Error
        text: appsPage.fileError
    }
    FormCard.FormHeader { title: "How profiles work" }
    FormCard.FormCard {
        FormCard.FormTextDelegate {
            text: "Each profile lists window classes (matched case-insensitively as substrings, so \"konsole\" covers \"org.kde.konsole\"). Profiles are tried top to bottom and the first match wins; a window no profile matches gets the neutral tone with cleanup on."
            textItem.wrapMode: Text.WordWrap
        }
    }

    Repeater {
        model: appsPage.profiles
        // A plain Column rather than a nested ColumnLayout: the form delegates
        // derive their implicit width from wrapping labels, and a nested layout
        // feeds that back into the page until Qt's layout engine spins.
        Item {
            id: card
            required property var modelData
            required property int index
            Layout.fillWidth: true
            implicitHeight: column.implicitHeight
            Column {
            id: column
            width: parent.width
            FormCard.FormHeader {
                width: parent.width
                title: (card.index + 1) + ". " + (card.modelData.name !== "" ? card.modelData.name : "Unnamed")
                trailing: RowLayout {
                    spacing: 0
                    QQC2.ToolButton {
                        icon.name: "arrow-up"
                        text: "Move up"
                        display: QQC2.AbstractButton.IconOnly
                        enabled: card.index > 0
                        QQC2.ToolTip.text: text
                        QQC2.ToolTip.visible: hovered
                        onClicked: appsPage.move(card.index, -1)
                    }
                    QQC2.ToolButton {
                        icon.name: "arrow-down"
                        text: "Move down"
                        display: QQC2.AbstractButton.IconOnly
                        enabled: card.index < appsPage.profiles.length - 1
                        QQC2.ToolTip.text: text
                        QQC2.ToolTip.visible: hovered
                        onClicked: appsPage.move(card.index, 1)
                    }
                    QQC2.ToolButton {
                        icon.name: "edit-delete"
                        text: "Remove"
                        display: QQC2.AbstractButton.IconOnly
                        QQC2.ToolTip.text: text
                        QQC2.ToolTip.visible: hovered
                        onClicked: appsPage.remove(card.index)
                    }
                }
            }
            FormCard.FormCard {
                width: parent.width
                FormCard.FormTextFieldDelegate {
                    label: "Name"
                    text: card.modelData.name
                    onEditingFinished: appsPage.set(card.index, "name", text.trim())
                }
                FormCard.FormDelegateSeparator {}
                FormCard.FormTextFieldDelegate {
                    label: "Window classes"
                    text: card.modelData.class.join(", ")
                    placeholderText: "konsole, kitty, alacritty"
                    onEditingFinished: appsPage.set(card.index, "class", appsPage.parseClasses(text))
                }
                FormCard.FormDelegateSeparator {}
                FormCard.FormComboBoxDelegate {
                    text: "Tone"
                    model: appsPage.toneLabels
                    currentIndex: Math.max(0, appsPage.tones.indexOf(card.modelData.tone))
                    onActivated: (i) => appsPage.set(card.index, "tone", appsPage.tones[i])
                }
                FormCard.FormDelegateSeparator {}
                FormCard.FormSwitchDelegate {
                    text: "Clean up with the model"
                    description: "Off types the transcript as heard, after dictionary replacements."
                    checked: card.modelData.cleanup
                    onToggled: appsPage.set(card.index, "cleanup", checked)
                }
                FormCard.FormDelegateSeparator {}
                FormCard.FormTextAreaDelegate {
                    label: "Instructions"
                    text: card.modelData.instructions
                    placeholderText: "Extra guidance for the cleanup model in this app"
                    onEditingFinished: appsPage.set(card.index, "instructions", text)
                }
            }
            }
        }
    }

    FormCard.FormHeader { title: "Try it" }
    FormCard.FormCard {
        FormCard.FormTextDelegate {
            text: "Run the cleanup on some text as if it were dictated into a window of a given class, without typing anything."
            textItem.wrapMode: Text.WordWrap
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextFieldDelegate {
            id: tryText
            label: "Text"
            placeholderText: "um so lets meet at 3 pm question mark"
            onAccepted: tryButton.clicked()
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextFieldDelegate {
            id: tryApp
            label: "Window class"
            placeholderText: "org.kde.konsole"
            onAccepted: tryButton.clicked()
        }
        FormCard.FormDelegateSeparator {}
        FormCard.AbstractFormDelegate {
            background: null
            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                QQC2.Button {
                    id: tryButton
                    text: appsPage.previewing ? "Cleaning up…" : "Preview"
                    icon.name: "media-playback-start"
                    enabled: Daemon.connected && !appsPage.previewing && tryText.text.trim() !== ""
                    onClicked: {
                        appsPage.previewing = true;
                        appsPage.previewResult = "";
                        Daemon.preview(tryText.text, tryApp.text);
                    }
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    visible: text !== ""
                    text: appsPage.previewResult !== "" ? appsPage.previewResult : (Daemon.connected ? "" : "Needs the daemon")
                    wrapMode: Text.WordWrap
                }
            }
        }
    }
    Item { Layout.preferredHeight: Kirigami.Units.largeSpacing }
}
