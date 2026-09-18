# parla

Hold a key, say what you want, and have it happen on a KDE Plasma desktop.

parla captures speech locally and transcribes it with whisper.cpp on the GPU.
Nothing leaves the machine. A small instruct model on the same GPU cleans up
dictation for the application it lands in, applies spoken edits like "make
that shorter", and reads the intent of commands the grammar cannot parse.
Sending either job to a cloud API instead is one line of config.

## How an utterance travels

```
hotkey held → PipeWire capture → energy gate → whisper.cpp → router → executor
                                  (trim, reject)   (CUDA)        │
                                                    dictation ───┼─ snippets, dictionary, cleanup → typed
                                                    command  ────┼─ grammar      in-process
                                                                 ├─ judged path  local GGUF, or API
                                                                 └─ refuse
```

Two hotkeys, both push-to-talk. `Ctrl+Space` dictates, and parla types the
transcript into whatever has focus, cleaned up for the application it lands
in. `Ctrl+Shift+Space` commands, and the router parses the transcript into an
action. The UI's overlay does the same with a click, for hands-free use.

The router tries the cheap path first. A grammar of 42 literal patterns matches
`open firefox` or `desktop two` without leaving the process. Anything phrased
outside those patterns falls through to the judged path, which asks a model for
the intent and its arguments in one evaluation. By default that model is a 4B
instruct GGUF running through llama.cpp on the GPU. `backend = "typesafe"`
under `[judge]` sends the same questions to the TypeSafe System One API. What
neither path can resolve, parla refuses out loud instead of guessing.

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

## Dictation

Whisper writes what it heard, fillers and all. Before the transcript is
typed, parla runs it through three things, in this order.

**Snippets.** An utterance that is a snippet's name types the snippet
instead: say "my email" or "insert my email" and the address comes out.
Snippets live in `~/.config/parla/snippets.toml`:

```toml
[[snippet]]
trigger = "my email"
text = "someone@example.com"
```

**The dictionary.** `~/.config/parla/dictionary.toml` holds the names and
jargon whisper gets wrong, and spoken-to-written replacements:

```toml
words = ["Emanuele", "KWin", "llama.cpp"]

[[replace]]
spoken = "e-mail"
written = "email"
```

The words prime whisper's decoder on every utterance, so it transcribes
them as spelled, and the cleanup model is told to spell them exactly so.
Replacements are deterministic, whole-word and case-insensitive, and run
before and after cleanup.

**Cleanup.** The model under `[local]` (the same Qwen3-4B the judged path
uses) rewrites the transcript: filler words and false starts go, a
self-correction keeps its final version ("send it Monday, no, Tuesday"
becomes "send it Tuesday"), punctuation and capitalisation get fixed, spoken
"new paragraph" becomes one, and an enumeration becomes a list. It is told
never to add, answer or translate, and its output is checked: empty or
suspiciously long, and the raw transcript is typed instead. So is the raw
transcript when the model is slower than `flow.timeout_ms`. On the RTX 5060
a sentence comes back in well under a second.

How the text should read depends on where it lands. `~/.config/parla/apps.toml`
maps window classes to profiles, first match wins:

```toml
[[app]]
name = "Terminals"
class = ["konsole", "kitty"]
tone = "code"          # neutral | casual | formal | code
cleanup = true
instructions = "Prefer snake_case identifiers."
```

The four tones are what the model is told about the register. `code` turns
"dash" and "underscore" into symbols and adds no trailing period. `casual`
keeps contractions and invents no sign-offs. `formal` writes complete
sentences and paragraphs. A fresh install starts with profiles for
terminals, editors, chat and mail, and a window no profile matches gets
`neutral`. `cleanup = false` on a profile types the transcript as heard.

```
parlad --flow "um so send it monday no tuesday" org.kde.konsole
```

runs one transcript through all of this for a given window class and prints
what would be typed, without typing it.

**Taking it back.** For a while after a dictation (`flow.edit_window_ms`,
90 s by default) the command hotkey accepts "scratch that", which deletes
what was typed, and free-form edits such as "make that more formal",
"shorter" or "turn that into bullet points". The judged path decides an
utterance is an edit only while a recent dictation exists and the focus has
not moved to another window. The model then rewrites the text and parla
replaces it. Edits act without a confirmation prompt, since the text is the
user's own words of a moment ago and can be dictated again.

`[flow] backend = "openai"` sends cleanup and edits to any server speaking
the OpenAI chat completions API instead, with `flow.openai.base_url` and
`OPENAI_API_KEY`. That covers OpenAI, OpenRouter, Groq, or a llama-server on
another machine.

## The UI

`parla-ui` is a Qt Quick and Kirigami application, so it looks like the
rest of Plasma. It talks to the daemon over the session bus, name
`org.parla.Daemon`, interface `org.parla.Daemon1`. The contract is in
`crates/parlad/dbus/org.parla.Daemon1.xml`.

- An overlay pill at the bottom of the screen appears while parla listens,
  with a live microphone level, then shows what happened: "typed 14 words",
  the confirmation question, or an error. Clicking it starts and stops a
  hands-free dictation.
- A tray icon shows the state and toggles parla on and off.
- The main window has the history of dictations with search and copy, the
  dictionary, the snippets, the app profiles with a "try it" box that runs
  the cleanup without typing, statistics (words per day, words per minute),
  and the settings paths.

The daemon owns the pipeline. The UI writes the three TOML files above and
asks the daemon to reload them, and reads history through the daemon. The
history itself is `~/.local/share/parla/history.jsonl`, one record per
utterance with the raw transcript, what was typed or done, the window
class, and timings. `flow.history = false` turns it off.

The daemon runs without the UI, and without a session bus at all.

The UI is Rust too, through cxx-qt, with a short C++ shim for the
application object and the tray. `parla-mockd` is a stand-in daemon that
serves the same interface with made-up state and history, so the UI can be
worked on without a microphone:

```
cargo build --release -p parla-ui
./target/release/parla-mockd &
./target/release/parla-ui
```

It refuses to start while a real parlad owns the bus name.

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

Measured with the TypeSafe backend on a 123-application, 12-window desktop:

| utterance | result | confidence |
| --- | --- | --- |
| `could you bring up firefox for me` | `LaunchApp { "Firefox" }` | 0.93 |
| `shut down the browser` | `CloseWindow { "Google Chrome" }` | 0.90, asks first |
| `I need a terminal` | `FocusWindow { "…Konsole" }` | 0.89 |
| `The quarterly report shows a modest increase.` | dictation, not a command | n/a |
| `what's the weather like tomorrow` | refused | n/a |

### Local by default

The judged path is on by default and runs `Qwen3-4B-Instruct-2507` at Q4_K_M
through llama.cpp, on the same GPU as whisper. `scripts/fetch-model.sh`
downloads it next to the whisper model, and `local.model_path` points at any
other GGUF instruct model. One copy of the model serves both the judged path
and dictation cleanup, on one thread, so the two never compete for VRAM.

The model never generates text. Every question has a closed set of answers, so
parlad scores each option instead. An option's score is the probability the
model assigns to replying with exactly that key and then ending its turn,
normalised over the set. That makes `a:1` and `a:12` separate outcomes rather
than one being a prefix of the other. A yes/no question is the two-option set
`yes`/`no`, and a choice question's confidence is the probability of the
winning key. parlad renders the desktop state once per utterance and keeps it
in the KV cache across the questions, then scores the options of each question
in one batch as parallel sequences. On an RTX 5060 an utterance against a
12-window desktop took 700 ms and 2000 prompt tokens.

The probabilities a 4B model produces are peaky. On the utterances tried so
far most answers came back at 1.00 or 0.00, with `bring up firefox` picking
Firefox at 0.80 the only middling one. The thresholds were chosen for
TypeSafe's calibrated outputs and have not been re-measured for this model.

Window titles always go into the questions here, since nothing leaves the
host. A 4B model at Q4 takes about 2.5 GB of VRAM plus the context.
`local.gpu_layers = 0` keeps it on the CPU.

### The TypeSafe backend

`backend = "typesafe"` under `[judge]` sends the same state and questions to
the TypeSafe System One API. Export `TYPESAFE_API_KEY`; parlad reads the key
from the environment ahead of `judge.typesafe.api_key`, so it never has to
touch disk. The request carries the utterance, the installed application
names, and for each open window its application class and an index. Window
titles stay on this machine unless `judge.send_window_titles = true`. The
model picks an index and parlad maps it back to the title here.

The thresholds under `[judge]` are per backend in practice. Switching backends
means measuring them again with `parlad --judge`, which prints every answer's
probability when run with `RUST_LOG=parlad=debug`.

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
- A CUDA toolchain. `whisper-rs` and `llama-cpp-2` are both built with their
  `cuda` feature, so the GPU is not optional.
- For the UI: Qt 6 with Qt Quick, Kirigami, `qqc2-desktop-style` and
  `layer-shell-qt`. The daemon needs none of these.
- `kdotool` for window control, and `tmux` for the Claude Code session.
- A whisper ggml model and a GGUF instruct model for the judged path. The
  default paths are `~/.local/share/parla/models/ggml-large-v3-turbo.bin` and
  `Qwen3-4B-Instruct-2507-Q4_K_M.gguf` in the same directory.

Typing uses KWin's EIS interface over libei, falling back to `ydotool` if that
probe fails. Launching tries `kioclient`, then `gtk-launch`, then
`systemd-run --user`.

## Getting it running

```
cargo build --release
cargo build --release -p parla-ui
```

The first builds the daemon and its libraries; the UI is a separate step
because it needs Qt. Put the models where the config expects them:

```
scripts/fetch-model.sh
```

downloads `ggml-large-v3-turbo.bin` from the whisper.cpp model releases and
`Qwen3-4B-Instruct-2507-Q4_K_M.gguf` from Hugging Face into
`~/.local/share/parla/models`, resumably, and is a no-op once the files are
there. `scripts/fetch-model.sh whisper <name>` or `judge <repo> <file>`
fetches a different one.

Write a config, then check the environment before starting the daemon:

```
parlad --print-default-config > ~/.config/parla/parla.toml
parlad --check
```

`--check` resolves both model paths, parses the dictionary, snippets and
app profiles, parses both hotkey chords, lists input devices, probes the
injectors, counts visible windows and virtual desktops, tries one `.desktop`
lookup, and says whether a daemon already owns the bus name. It registers no
hotkeys and loads no model, so it is safe to run against a live session.

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
| `parla-flow` | Dictionary, snippets, app profiles and history: the files both the daemon and the UI read |
| `parlad` | The daemon: capture, VAD, ASR, hotkeys, router, policy, confirmation, dictation cleanup, judged path, session bus |
| `parla-ui` | The Kirigami UI: overlay, tray icon, history, dictionary, snippets, app profiles |

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
- **Voice edits replace text by backspacing.** "Scratch that" deletes as
  many characters as parla typed. An application that rewrote the text in
  the meantime (autocorrect, autocomplete) leaves a mess.
- **Cleanup does not see the screen.** Wispr Flow reads the text around the
  cursor to match its style; parla knows only the window class.
