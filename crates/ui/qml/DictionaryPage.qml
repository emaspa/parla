import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.parla.ui

// The personal dictionary: words whisper should spell right, and
// spoken-to-written replacements. Saved on every change, then reloaded
// by the daemon.
FormCard.FormCardPage {
    id: page
    title: "Dictionary"

    property var words: []
    property var replacements: []
    property string fileError: ""

    function load() {
        const json = Store.loadDictionary();
        if (json === "") {
            fileError = Store.lastError;
            return;
        }
        fileError = "";
        const d = JSON.parse(json);
        words = d.words || [];
        replacements = d.replace || [];
    }
    function save() {
        const ok = Store.saveDictionary(JSON.stringify({ words: words, replace: replacements }));
        fileError = ok ? "" : Store.lastError;
        if (ok) {
            Daemon.reload();
        }
    }
    function addWord(w) {
        w = w.trim();
        if (w === "" || words.some(x => x.toLowerCase() === w.toLowerCase())) return;
        words = words.concat([w]);
        save();
    }
    function removeWord(i) {
        words = words.filter((_, k) => k !== i);
        save();
    }
    function addReplacement(spoken, written) {
        spoken = spoken.trim();
        if (spoken === "") return;
        replacements = replacements.concat([{ spoken: spoken, written: written.trim() }]);
        save();
    }
    function updateReplacement(i, spoken, written) {
        // In place, so the rows are not rebuilt under the cursor.
        if (replacements[i].spoken === spoken && replacements[i].written === written) return;
        replacements[i].spoken = spoken;
        replacements[i].written = written;
        save();
    }
    function removeReplacement(i) {
        replacements = replacements.filter((_, k) => k !== i);
        save();
    }
    Component.onCompleted: load()

    actions: [
        Kirigami.Action {
            text: "Open file"
            icon.name: "document-edit"
            onTriggered: Store.openInEditor(Store.dictionaryFile)
        }
    ]


    DaemonBanner {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.largeSpacing
    }
    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.largeSpacing
        visible: page.fileError !== ""
        type: Kirigami.MessageType.Error
        text: page.fileError
    }

    FormCard.FormHeader { title: "Words" }
    FormCard.FormCard {
        FormCard.AbstractFormDelegate {
            background: null
            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                QQC2.Label {
                    text: "Names, jargon and acronyms as they should be written. The transcriber is biased toward these spellings and the cleanup model keeps them exact."
                    wrapMode: Text.WordWrap
                    opacity: 0.7
                    Layout.fillWidth: true
                }
                Kirigami.ActionTextField {
                    id: wordField
                    Layout.fillWidth: true
                    placeholderText: "Add a word and press Enter"
                    onAccepted: { page.addWord(text); text = ""; }
                    rightActions: [
                        Kirigami.Action {
                            icon.name: "list-add"
                            text: "Add"
                            visible: wordField.text.trim() !== ""
                            onTriggered: { page.addWord(wordField.text); wordField.text = ""; }
                        }
                    ]
                }
                Flow {
                    Layout.fillWidth: true
                    Layout.topMargin: Kirigami.Units.smallSpacing
                    spacing: Kirigami.Units.smallSpacing
                    Repeater {
                        model: page.words
                        Kirigami.Chip {
                            required property string modelData
                            required property int index
                            text: modelData
                            closable: true
                            checkable: false
                            onRemoved: page.removeWord(index)
                        }
                    }
                    QQC2.Label {
                        visible: page.words.length === 0
                        text: "No words yet."
                        opacity: 0.6
                    }
                }
            }
        }
    }

    FormCard.FormHeader { title: "Replacements" }
    FormCard.FormCard {
        FormCard.AbstractFormDelegate {
            background: null
            contentItem: QQC2.Label {
                text: "Whole-word, case-insensitive substitutions applied to every dictation: what you say on the left, what gets typed on the right."
                wrapMode: Text.WordWrap
                opacity: 0.7
            }
        }
        Repeater {
            model: page.replacements
            FormCard.AbstractFormDelegate {
                id: rrow
                required property var modelData
                required property int index
                background: null
                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing
                    QQC2.TextField {
                        id: spokenField
                        Layout.fillWidth: true
                        text: rrow.modelData.spoken
                        placeholderText: "spoken"
                        onEditingFinished: page.updateReplacement(rrow.index, text, writtenField.text)
                    }
                    Kirigami.Icon {
                        source: "arrow-right"
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                    }
                    QQC2.TextField {
                        id: writtenField
                        Layout.fillWidth: true
                        text: rrow.modelData.written
                        placeholderText: "written"
                        onEditingFinished: page.updateReplacement(rrow.index, spokenField.text, text)
                    }
                    QQC2.ToolButton {
                        icon.name: "edit-delete"
                        text: "Remove"
                        display: QQC2.AbstractButton.IconOnly
                        QQC2.ToolTip.text: text
                        QQC2.ToolTip.visible: hovered
                        onClicked: page.removeReplacement(rrow.index)
                    }
                }
            }
        }
        FormCard.AbstractFormDelegate {
            background: null
            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing
                QQC2.TextField {
                    id: newSpoken
                    Layout.fillWidth: true
                    placeholderText: "spoken, e.g. e-mail"
                    onAccepted: newWritten.forceActiveFocus()
                }
                Kirigami.Icon {
                    source: "arrow-right"
                    Layout.preferredWidth: Kirigami.Units.iconSizes.small
                    Layout.preferredHeight: Kirigami.Units.iconSizes.small
                }
                QQC2.TextField {
                    id: newWritten
                    Layout.fillWidth: true
                    placeholderText: "written, e.g. email"
                    onAccepted: addButton.clicked()
                }
                QQC2.ToolButton {
                    id: addButton
                    icon.name: "list-add"
                    text: "Add"
                    enabled: newSpoken.text.trim() !== ""
                    onClicked: {
                        page.addReplacement(newSpoken.text, newWritten.text);
                        newSpoken.text = "";
                        newWritten.text = "";
                        newSpoken.forceActiveFocus();
                    }
                }
            }
        }
    }
}
