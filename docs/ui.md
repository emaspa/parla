# The desktop UI

`parla-ui` is a Qt Quick application built with Kirigami, so it takes
Plasma's theme, fonts and widget style and looks like the rest of the
desktop. It is a client of the daemon over the session bus and does
nothing on its own: without a running parlad it shows a banner saying so
and the pages that need the daemon stay empty. The daemon does not need it.

## What it shows

**The overlay.** A pill at the bottom of the screen, on the layer-shell
overlay layer so it floats above windows without taking focus. It appears
when a capture starts, with a microphone icon, live level bars and the mode.
While whisper and the model run it says so. When the utterance is done it
flashes the result ("typed 14 words", "Launched Firefox", or the error in
red) and fades. While a confirmation prompt is up it shows the question and
stays until the answer.

Clicking the pill while recording stops the capture and processes it, like
releasing the key. Right-clicking cancels. Clicking the result flash starts
a hands-free dictation, and so does clicking the small idle dot that the
Settings page can enable, so a mouse alone can dictate: click, speak,
click. The overlay is a separate top-level window, not a child of the main
one, so it stays when the main window is closed. It cannot take an Escape
key without stealing focus from the window being dictated into, which is
why cancel is a right-click.

**The tray icon.** A status notifier item that follows the daemon's state.
Left-click opens the main window, middle-click starts a dictation. The menu
has Open parla, Start dictation, an Enabled toggle that maps to the
daemon's `Enabled` property, and Quit. When no daemon is on the bus the
item switches to its attention state.

**Home.** The daemon's state and a button that starts and stops hands-free
dictation, the two hotkeys, which models judge and clean up, the daemon's
version, stat tiles with the totals and words per minute, a bar chart of words per day for the last 30 days, and the most recent
utterances with their app and outcome.

**History.** Every stored utterance, newest first, with a search box over
text, app and window title. Each entry shows what whisper heard and what
was typed or done, the application, the profile, timings and the outcome,
with "Copy text", "Copy raw transcript" and "Delete". "Clear all" at the
top asks once before deleting everything. The list is fetched through the
daemon in pages.

**Dictionary.** The word list and the replacement pairs, editable in place.
Save writes `dictionary.toml` and asks the daemon to reload; a parse error
from the daemon appears as a notification with the file and line.

**Snippets.** Trigger and text pairs, with a multi-line editor for the text.

**Apps.** The profiles in order, each with its name, window classes, tone
(a combo of neutral, casual, formal, code), a cleanup switch and free-text
instructions. Profiles can be added, removed and reordered, since the first
match wins. A "try it" box at the bottom takes any text and a window class
and sends them through the daemon's `Preview`, so you can see what the
matching profile would have typed.

**Settings.** The Enabled switch, the daemon's status and version, the
hotkeys and models as read from the daemon, a button that opens
`parla.toml` in the desktop's editor, "Show a dot while idle", "Start with
the session", which tray backend is in use, and buttons that open the three
TOML files in an editor.

"Start with the session" writes `~/.config/autostart/parla-ui.desktop`
with `Exec=parla-ui --hidden`, so the tray and overlay come up at login with
the main window closed. Switching it off deletes the file. This starts the
UI only; the daemon is started separately, see
[getting-started.md](getting-started.md).

## Running it

```
parla-ui            # main window, tray and overlay
parla-ui --hidden   # tray and overlay only; --tray is the same
parla-ui --version
```

The main window can be closed and reopened from the tray without the
overlay going away. Quit is in the tray menu.

The only per-viewer preference the UI keeps itself is the idle dot, in the
usual Qt settings location under the `overlay` group. Everything else is
either the daemon's (state, history) or one of the TOML files.

## How it is built

The application is Rust. cxx-qt exposes two QML singletons: `Daemon`, the
bus client, whose properties mirror the daemon's and whose invokables map
one to one onto its methods, and `Store`, which loads and saves the three
TOML files through the same parla-flow types the daemon uses, so a file the
UI wrote and a file edited by hand parse the same way. A short C++ shim
creates the `QApplication`, the tray item and the QML engine. The QML
lives in `crates/ui/qml` and is compiled into the binary, so the release
build is one self-contained executable.

The bus client runs on a tokio thread and forwards property changes and
signals to the Qt thread as events. Calls from QML are queued the other way
and answered with signals (`historyLoaded`, `previewReady`, `reloaded`),
so the UI never blocks on the daemon.

The build needs, beyond a Rust toolchain:

- Qt 6 with the Gui, Widgets, Qml, Quick and QuickControls2 modules, found
  through `qmake6` on the path or the `QMAKE` environment variable.
- Kirigami, Kirigami Addons (for the form cards), and `qqc2-desktop-style`
  so controls look like Breeze.
- KStatusNotifierItem and KI18n from KDE Frameworks 6.
- `layer-shell-qt` for the overlay layer on Wayland.

`build.rs` checks each of these before compiling anything and fails with a
message naming the missing piece. `cargo build` at the workspace root skips
the UI on purpose; build it with `cargo build --release -p parla-ui`.

Three environment variables exist for testing without a person at the
screen. `PARLA_UI_PAGE=history` opens on that page. `PARLA_UI_SHOT=/tmp/ui`
saves a screenshot of the window and of each overlay phase under that
prefix. `PARLA_UI_SELFTEST=1` walks through the pages as clicks would, and is
what was used to check the pages against `parla-mockd`.
