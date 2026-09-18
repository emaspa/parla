import QtQuick
import QtQuick.Layouts
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.parla.ui

// Snippets: say the trigger (alone, or after "insert", "paste", "type",
// "put in") and the text is typed instead.
FormCard.FormCardPage {
    id: page
    title: "Snippets"

    property var snippets: []
    property string fileError: ""

    function load() {
        const json = Store.loadSnippets();
        if (json === "") {
            fileError = Store.lastError;
            return;
        }
        fileError = "";
        snippets = JSON.parse(json).snippet || [];
    }
    function save() {
        const ok = Store.saveSnippets(JSON.stringify({ snippet: snippets }));
        fileError = ok ? "" : Store.lastError;
        if (ok) {
            Daemon.reload();
        }
    }
    function add() {
        snippets = snippets.concat([{ trigger: "", text: "" }]);
    }
    function update(i, trigger, text) {
        if (snippets[i].trigger === trigger && snippets[i].text === text) return;
        snippets[i].trigger = trigger;
        snippets[i].text = text;
        if (trigger.trim() !== "") {
            save();
        }
    }
    function remove(i) {
        snippets = snippets.filter((_, k) => k !== i);
        save();
    }
    Component.onCompleted: load()

    actions: [
        Kirigami.Action {
            text: "Add snippet"
            icon.name: "list-add"
            onTriggered: page.add()
        },
        Kirigami.Action {
            text: "Open file"
            icon.name: "document-edit"
            onTriggered: Store.openInEditor(Store.snippetsFile)
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
    FormCard.FormHeader { title: "How snippets work" }
    FormCard.FormCard {
        FormCard.FormTextDelegate {
            text: "Say the trigger on its own, or with \"insert\", \"paste\", \"type\" or \"put in\" before it, and the text is typed in its place. Case and punctuation do not matter. Ordinary sentences that happen to contain a trigger are left alone."
            textItem.wrapMode: Text.WordWrap
        }
    }

    Kirigami.PlaceholderMessage {
        visible: page.snippets.length === 0
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.gridUnit * 3
        icon.name: "edit-paste"
        text: "No snippets yet"
        explanation: "Add one for your email address, a signature, a command you type often."
        helpfulAction: Kirigami.Action {
            text: "Add snippet"
            icon.name: "list-add"
            onTriggered: page.add()
        }
    }

    Repeater {
        model: page.snippets
        ColumnLayout {
            id: card
            required property var modelData
            required property int index
            Layout.fillWidth: true
            spacing: 0
            FormCard.FormHeader {
                title: card.modelData.trigger !== "" ? "“" + card.modelData.trigger + "”" : "New snippet"
                trailing: QQC2.ToolButton {
                    icon.name: "edit-delete"
                    text: "Remove"
                    display: QQC2.AbstractButton.IconOnly
                    QQC2.ToolTip.text: text
                    QQC2.ToolTip.visible: hovered
                    onClicked: page.remove(card.index)
                }
            }
            FormCard.FormCard {
                FormCard.FormTextFieldDelegate {
                    id: triggerField
                    label: "Say"
                    text: card.modelData.trigger
                    placeholderText: "my email"
                    onEditingFinished: page.update(card.index, text, textArea.text)
                }
                FormCard.FormDelegateSeparator {}
                FormCard.FormTextAreaDelegate {
                    id: textArea
                    label: "Insert"
                    text: card.modelData.text
                    placeholderText: "someone@example.com"
                    onEditingFinished: page.update(card.index, triggerField.text, text)
                }
            }
        }
    }
    Item { Layout.preferredHeight: Kirigami.Units.largeSpacing }
}
