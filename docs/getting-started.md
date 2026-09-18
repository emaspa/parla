# Getting started

From a fresh KDE Plasma 6 machine with an NVIDIA GPU to dictating, in six
steps. Each step ends with a way to check it worked.

## 1. Requirements

- KDE Plasma 6 on Wayland or X11. The daemon needs kglobalaccel for the
  hotkeys and KWin's D-Bus interfaces for windows, desktops and typing.
- An NVIDIA GPU with a CUDA toolchain installed. whisper-rs and llama-cpp-2
  are both built with their `cuda` feature, and the build fails without
  `nvcc`. A 4B model at Q4 plus large-v3-turbo fit in 8 GB of VRAM with room
  for the desktop.
- `kdotool` for window control and `tmux` for the Claude Code intents.
- Optionally `ydotool` with its user service running, as the fallback
  injector when KWin's EIS interface is not available.
- For the UI: Qt 6, Kirigami, Kirigami Addons, `qqc2-desktop-style`,
  KStatusNotifierItem, KI18n and `layer-shell-qt`. The daemon needs none of
  these.

On Arch and derivatives the package names are `cuda`, `kdotool` (AUR),
`tmux`, `ydotool`, `qt6-declarative`, `kirigami`, `kirigami-addons`,
`qqc2-desktop-style`, `kstatusnotifieritem`, `ki18n` and `layer-shell-qt`.
Other distributions package the same libraries under similar names.

## 2. Build

```
cargo build --release
cargo build --release -p parla-ui
```

The first builds `parlad`, `parla-probe` and the libraries; it compiles
whisper.cpp and llama.cpp with CUDA, which takes several minutes the first
time. The second builds `parla-ui` and `parla-mockd`, and is separate
because it needs Qt. If a Qt or KDE piece is missing, `build.rs` says which
one before anything is compiled.

Binaries land in `target/release`. Put `parlad` and `parla-ui` somewhere on
your `PATH`, or run them from there. The audio cues are found next to the
binary under `cues/` or `../share/parla/cues/`, or under
`~/.local/share/parla/cues/`; copy `crates/parlad/assets/cues/*.wav` to one
of those if you want them.

## 3. Models

```
scripts/fetch-model.sh
```

downloads `ggml-large-v3-turbo.bin` from the whisper.cpp releases and
`Qwen3-4B-Instruct-2507-Q4_K_M.gguf` from Hugging Face into
`~/.local/share/parla/models`, resumably, and is a no-op once the files are
there. About 4 GB in total. `scripts/fetch-model.sh whisper ggml-medium` or
`scripts/fetch-model.sh judge <repo> <file.gguf>` fetch a different one;
point `asr.model_path` or `local.model_path` at it.

## 4. Configure

```
mkdir -p ~/.config/parla
parlad --print-default-config > ~/.config/parla/parla.toml
```

The defaults work as they are. The things most people change on day one:

- `hotkeys.dictate` and `hotkeys.command`, if `ctrl+space` is taken by an
  input method or an editor.
- `asr.language`, if you dictate in something other than English.
- `audio.device`, if the default input is not the microphone you want.
  `parlad --check` lists the names.

Every key is described in [configuration.md](configuration.md).

## 5. Check

```
parlad --check
```

resolves both model paths, parses the dictionary, snippets and app profiles,
parses both hotkey chords, lists input devices, probes the injectors and
says which one is active, counts visible windows and virtual desktops,
resolves one `.desktop` lookup, reports whether the session is locked and
whether a daemon already owns the bus name. It registers no hotkeys, loads
no model and types nothing, so it is safe on a live session.

Two more read-only commands are worth knowing before the first real
utterance:

```
parlad --flow "um so send it monday no tuesday" org.kde.konsole
parlad --judge "bring the file manager to the front"
```

The first loads the model and runs one transcript through cleanup for a
window class, printing what would be typed. The second runs one command
through the grammar and the judge against your actual windows and prints
what would happen and whether it would ask first. Both take a few seconds
the first time while the model loads.

## 6. Run

```
parlad
```

logs where the config came from, which injector it picked, the hotkeys it
registered, and "on the session bus as org.parla.Daemon". Then hold
`ctrl+space`, say something into a text field, release. The first
transcription takes longer while whisper warms up.

```
parla-ui
```

in another terminal shows the overlay and the tray icon, and the main
window with the history filling up as you dictate. In Settings, "Start with
the session" makes the UI come up at login.

To have the daemon come up at login too, a user service works well, since
parlad exits on purpose when it loses its hotkeys and wants restarting:

```ini
# ~/.config/systemd/user/parlad.service
[Unit]
Description=parla voice daemon
After=graphical-session.target
PartOf=graphical-session.target

[Service]
ExecStart=%h/.local/bin/parlad
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info

[Install]
WantedBy=graphical-session.target
```

```
systemctl --user daemon-reload
systemctl --user enable --now parlad
journalctl --user -u parlad -f
```

Put `TYPESAFE_API_KEY` or `OPENAI_API_KEY` in a `~/.config/environment.d`
file rather than in the unit, if you use either backend.

## When something is off

**"hotkey ... is taken by another shortcut".** kglobalaccel bound something
else because another component owns the chord. System Settings, Shortcuts,
shows who. Change `[hotkeys]` or free the chord.

**"another parlad is already running".** The lock file names the first
instance. Stop it, or check `systemctl --user status parlad` if it runs as
a service.

**Nothing is typed, no error.** Check the session is not locked and that
`--check` reports an active injector. On Wayland the EIS probe needs a
KWin that exposes the interface; on failure `ydotool` is tried, which needs
`ydotool.service` running as your user. `parla-probe type hello` types into
the focused window and reports the injector's error directly.

**Whisper writes "Thank you." on silence.** That is the hallucination the
blocklist and the energy gate exist for. Raise `audio.speech_threshold` a
little if the room is noisy.

**A name keeps coming out wrong.** Add it to `words` in the dictionary, or
to the Dictionary page. It primes whisper and instructs the cleanup model.
If it is still wrong, add a `replace` entry for what whisper actually
writes.

**Cleanup is slow or falls back to the raw transcript.** Cleanup gives up
after `flow.timeout_ms`. Look for "cleanup failed" or "cleanup produced" in
the log. A CPU-only model (`local.gpu_layers = 0`) will not make the
default timeout; raise it or use the GPU.

**CUDA out of memory.** Something else has the GPU. Lower
`local.gpu_layers` to offload part of the model, or use a smaller GGUF.

**A command is refused as "unclear" that should have worked.** Run
`parlad --judge "<what you said>"` with `RUST_LOG=parlad=debug` to see the
probabilities. If the intent is right but below `judge.min_confidence`,
that is the threshold to tune; the defaults were set for the TypeSafe
backend, not the local model.

**The UI says the daemon is not running but it is.** The daemon logs "on
the session bus as org.parla.Daemon" when it owns the name. If it does not,
the session bus is missing from its environment, which happens when it is
started from a shell that lacks `DBUS_SESSION_BUS_ADDRESS`. A user service
gets it from systemd.
