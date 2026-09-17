# parla

Hold a key, say what you want, and have it happen on a KDE Plasma desktop.

parla captures speech locally and transcribes it with whisper.cpp on the GPU.
Nothing leaves the machine unless an utterance falls through to the judged path,
which is off by default.

## How an utterance travels

```
hotkey held → PipeWire capture → energy gate → whisper.cpp → router → executor
                                  (trim, reject)   (CUDA)        │
                                                                 ├─ grammar      in-process
                                                                 ├─ judged path  ~600 ms, network
                                                                 └─ refuse
```

Two hotkeys, both push-to-talk. `Ctrl+Space` dictates, and parla types the
transcript into whatever has focus. `Ctrl+Shift+Space` commands, and the router
parses the transcript into an action.

The router tries the cheap path first. A grammar of 42 literal patterns matches
`open firefox` or `desktop two` without leaving the process. Anything phrased
outside those patterns falls through to the judged path, which asks a TypeSafe
System One model for the intent and its arguments in one request. What neither
path can resolve, parla refuses out loud instead of guessing.

Either path ends in the same policy, and what the policy wants confirmed
waits for a spoken answer. parla resolves the target first, so the prompt
names the window rather than the words: "Close 'build — Konsole'? say yes".
The next command-mode utterance is matched against a short list of replies
before anything else. `yes`, `confirm`, `do it` or `go ahead` run the command;
`no`, `cancel`, `stop` or `never mind` drop it; any other command drops it
too, since the user has moved on. A yes is honoured for eight seconds
(`router.confirm_window_ms`) and only if the target is still the window the
prompt named. parla resolves the query again and requires the same window id.
A window that closed, or a focus that moved in the meantime, refuses instead
of closing something else. Dictation in between leaves the prompt alone.

## The judged path

The grammar matches word sequences, so `open firefox` works and `could you
bring up firefox for me` does not. The judged path covers the second kind.

It sends one request carrying the utterance plus the state code already knows:
which applications are installed, which windows are open, how many virtual
desktops exist. It asks every question at once, including ones whose answers it
throws away, because the model evaluates independent questions in parallel and
they cost only their tokens.

Code decides everything code can decide.

- **Candidates come from real state.** The model picks an application from the
  ones installed and a window from the ones open, so it cannot name a target
  that does not exist. On a one-desktop machine, "the third desktop" has no
  candidate to choose and nothing happens. The grammar would emit
  `VirtualDesktop { n: 3 }` and fail at execution instead.
- **Launch or focus is not a judgment.** Whether something already runs is an
  observed fact. Asking the model to choose split its probability between two
  spellings of one wish. Letting code read the window list instead moved "bring
  the file manager to the front" from 0.44 to 0.78.
- **The payload gets copied, never rewritten.** For `tell claude to rerun the
  test`, the candidates are the trailing spans of the utterance, so the message
  reaches Claude Code exactly as spoken.

Confirmation gates on consequence rather than on which enum variant the router
built. Closing a terminal running a build and closing a calculator are both
`CloseWindow`. Confidence that parla understood an utterance is not permission
to act on it, so anything judged destructive always asks, and low confidence
asks even for harmless actions.

Measured on a 123-application, 12-window desktop:

| utterance | result | confidence |
| --- | --- | --- |
| `could you bring up firefox for me` | `LaunchApp { "Firefox" }` | 0.93 |
| `shut down the browser` | `CloseWindow { "Google Chrome" }` | 0.90, asks first |
| `I need a terminal` | `FocusWindow { "…Konsole" }` | 0.89 |
| `The quarterly report shows a modest increase.` | dictation, not a command | n/a |
| `what's the weather like tomorrow` | refused | n/a |

The path is off by default. Turn it on with `enabled = true` under
`[typesafe]` and export `TYPESAFE_API_KEY`. parlad reads the key from the
environment ahead of the config file, so it never has to touch disk. The
request carries the utterance, the installed application names, and for each
open window its application class and an index. Window titles stay on this
machine unless `send_window_titles = true`; the model picks an index and the
title is mapped back here.

The judge remembers which window or `.desktop` entry a candidate came from,
and that id is what gets executed. The title is never fuzzy-matched a second
time, so two windows with the same caption cannot swap places between judging
and acting.

```
parlad --judge "bring the file manager to the front"
```

runs one utterance end to end and prints what would happen without doing it.
It builds the same executor and takes the same snapshot the daemon would, so
the windows and desktops it reasons about are the live ones, and it prints the
command it would run and the policy's decision. The default config ships
hand-picked thresholds, and this is how to replace them with measured ones.

## Requirements

- KDE Plasma 6, Wayland or X11. The daemon talks to `kglobalaccel` for hotkeys
  and KWin over D-Bus for virtual desktops.
- A CUDA toolchain. `whisper-rs` is built with its `cuda` feature, so the GPU is
  not optional.
- `kdotool` for window control, and `tmux` for the Claude Code session.
- A whisper ggml model. The default path is
  `~/.local/share/parla/models/ggml-large-v3-turbo.bin`.

Typing uses KWin's EIS interface over libei, falling back to `ydotool` if that
probe fails. Launching tries `kioclient`, then `gtk-launch`, then
`systemd-run --user`.

## Getting it running

```
cargo build --release
```

Put a whisper model where the config expects one:

```
scripts/fetch-model.sh
```

downloads `ggml-large-v3-turbo.bin` from the whisper.cpp model releases into
`~/.local/share/parla/models`, resumably, and is a no-op once the file is
there. Pass another model name to fetch that one instead.

Write a config, then check the environment before starting the daemon:

```
parlad --print-default-config > ~/.config/parla/parla.toml
parlad --check
```

`--check` resolves the model path, parses both hotkey chords, lists input
devices, probes the injectors, counts visible windows and virtual desktops, and
tries one `.desktop` lookup. It registers no hotkeys and loads no model, so it
is safe to run against a live session.

Only one parlad runs per session. It takes a lock on
`$XDG_STATE_HOME/parla/parlad.lock` at startup, and a second instance exits
with a message naming the first. Two daemons would register the same hotkeys
and both type every utterance. A hotkey held longer than `audio.max_hold_ms`
(30 s by default) counts as released, so a lost release event cannot record
for ever. parlad finishes the capture and processes it as if the key had come
up.

## Layout

| crate | what it holds |
| --- | --- |
| `parla-grammar` | Intents and the literal-pattern grammar, including the confirm/deny replies |
| `desktopd` | The executor and its `Command` vocabulary: windows, launching, the `.desktop` index, virtual desktops, tmux, text injection |
| `parlad` | The daemon: capture, VAD, ASR, hotkeys, router, policy, confirmation, judged path |

`desktopd` is one implementation with two callers. It executes its own
`Command` type, whose window targets are a query, a window id or the focused
window, and it does not depend on the grammar; parlad maps intents onto
commands. The router calls it directly as a Rust library today, and its shape
lets an MCP server expose the same commands later.

## Not done yet

- **The judged path's thresholds are guesses.** They are hand-picked defaults.
  Nobody has checked them against a corpus of real utterances.
- **The energy gate is not a VAD.** It trims silence and rejects stray taps by
  RMS. A frame-level model can replace it behind the same interface.
- **No tray icon.** Audio cues mark recording start, stop, and failure instead.
