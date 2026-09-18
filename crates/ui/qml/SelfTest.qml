import QtQuick
import org.parla.ui

// A scripted walk-through of the UI's actions, driven by a timer so the
// asynchronous daemon replies land between steps. Enabled with
// PARLA_UI_SELFTEST=1; meant to run against parla-mockd with a scratch
// XDG_CONFIG_HOME. Logs "selftest:" lines for the shell to check.
Item {
    id: test
    property bool running: false
    property int step: 0
    visible: false
    onRunningChanged: if (running) console.info("selftest: starting")

    Timer {
        running: test.running
        interval: 1500
        repeat: true
        onTriggered: test.next()
    }

    readonly property var win: applicationWindow()
    function page() { return win.pageStack.currentItem; }

    Connections {
        target: Daemon
        enabled: test.running
        function onReloaded(ok, message) { console.info("selftest: reloaded", ok, message); }
        function onPreviewReady(cleaned) { console.info("selftest: preview:", cleaned); }
        function onHistoryDeleted(id) { console.info("selftest: deleted", id); }
        function onError(message) { console.info("selftest: error:", message); }
        function onEnabledChanged() { console.info("selftest: enabled ->", Daemon.enabled); }
        function onStateChanged() { console.info("selftest: state ->", Daemon.state); }
    }

    readonly property var steps: [
        () => win.navigate("dictionary"),
        () => { page().addWord("Selftest"); page().addReplacement("self test", "selftest"); },
        () => { page().removeWord(0); },
        () => win.navigate("snippets"),
        () => { page().add(); page().update(page().snippets.length - 1, "self test snippet", "hello from selftest"); },
        () => win.navigate("apps"),
        () => {
            const p = page();
            p.add();
            const i = p.profiles.length - 1;
            p.set(i, "name", "Selftest");
            p.set(i, "class", p.parseClasses("selftest, org.kde.dolphin"));
            p.set(i, "tone", "code");
            p.set(i, "cleanup", false);
            p.move(i, -1);
        },
        () => Daemon.preview("hello world", "selftest"),
        () => win.navigate("settings"),
        () => { Store.autostart = true; console.info("selftest: autostart", Store.autostart, Store.lastError); },
        () => { win.idleDot = true; },
        () => { Daemon.enabled = false; },
        () => { Daemon.enabled = true; },
        () => win.navigate("history"),
        () => { const p = page(); console.info("selftest: history records", p.records.length); if (p.records.length > 0) Daemon.deleteHistory(p.records[0].id); },
        () => { console.info("selftest: history records after delete", page().records.length); Daemon.start("dictate"); },
        () => Daemon.stop(),
        () => win.navigate("home"),
        () => { console.info("selftest: done"); test.running = false; }
    ]

    function next() {
        if (step >= steps.length) {
            running = false;
            return;
        }
        console.info("selftest: step", step);
        try {
            steps[step]();
        } catch (e) {
            console.info("selftest: step", step, "failed:", e);
        }
        step += 1;
    }
}
