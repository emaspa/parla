# Configuration

parlad reads one file, `~/.config/parla/parla.toml`, at startup. Every key
has a default, so an empty file and a missing file behave the same. Unknown
keys are an error: a typo in a section or key name stops the daemon with a
message naming the file, rather than silently applying a default.

```
parlad --print-default-config > ~/.config/parla/parla.toml
```

writes the defaults with their current values, including the model paths
resolved for this machine. Edit what you want and delete the rest, or keep
the whole file as documentation.

Changes to `parla.toml` need a daemon restart. The three files the UI edits
(dictionary, snippets, app profiles) are re-read on `Reload` over the bus or
by restarting; see [dictation.md](dictation.md).

## Environment

| variable | effect |
| --- | --- |
| `TYPESAFE_API_KEY` | API key for the TypeSafe judge backend. Wins over `judge.typesafe.api_key`. |
| `OPENAI_API_KEY` | API key for the OpenAI-compatible cleanup backend. Wins over `flow.openai.api_key`. |
| `RUST_LOG` | Log filter, e.g. `parlad=debug`. Default `info`. |
| `XDG_CONFIG_HOME` | Where `parla/` config files live. Default `~/.config`. |
| `XDG_DATA_HOME` | Where `parla/models` and `parla/history.jsonl` live. Default `~/.local/share`. |
| `XDG_STATE_HOME` | Where the single-instance lock lives. Default `~/.local/state`. |
| `PARLA_LAST_DICTATION` | Set to anything to make `parlad --judge` behave as if text was dictated a moment ago, so edit phrases can be tested. |
| `PARLA_CONTEXT_BEFORE` | Text `parlad --flow` treats as what is before the cursor, so the context section of the cleanup prompt can be tried without a live field. |

## Files

| path | contents |
| --- | --- |
| `~/.config/parla/parla.toml` | This file. |
| `~/.config/parla/dictionary.toml` | Words to prime whisper with, and spoken-to-written replacements. |
| `~/.config/parla/snippets.toml` | Phrases that expand to fixed text. |
| `~/.config/parla/apps.toml` | Per-application cleanup profiles. Written on first save; until then the built-in defaults apply. |
| `~/.local/share/parla/models/` | The whisper, Silero VAD and GGUF models, where `scripts/fetch-model.sh` puts them. |
| `~/.local/share/parla/history.jsonl` | One JSON record per utterance, when `flow.history` is on. |
| `~/.local/state/parla/parlad.lock` | Held with `flock` while a daemon runs. |
| `~/.config/autostart/parla-ui.desktop` | Written by the UI's "Start with the session" switch. |

## `[hotkeys]`

```toml
[hotkeys]
dictate = "ctrl+space"
command = "ctrl+shift+space"
```

Both are push-to-talk: capture runs while the chord is held. A chord is
modifiers and one main key joined by `+`, case-insensitive. Modifiers are
`ctrl` (or `control`), `shift`, `alt`, and `meta` (or `super`, `win`). The
main key is one of `space`, `enter`/`return`, `tab`, `escape`/`esc`,
`backspace`, `delete`/`del`, `up`, `down`, `left`, `right`, `f1` to `f12`, a
single letter or digit, or a single punctuation character. Two main keys in
one chord, or none, fail validation at startup.

parlad registers the chords with kglobalaccel under the component `parla`.
kglobalaccel answers with what it actually bound. When another application
already owns the chord the answer differs from the request, and parlad
exits with a message saying so instead of running deaf. Pick a chord that
System Settings shows as free.

## `[audio]`

```toml
[audio]
vad_threshold = 0.5
speech_threshold = 0.01
max_utterance_ms = 30000
min_utterance_ms = 250
max_hold_ms = 30000
```

`device` (absent by default) names the cpal input device. `parlad --check`
lists the names it can see. Absent means the system default. Capture runs
at the device's own rate and is downmixed and resampled to the 16 kHz mono
whisper expects.

The gate in front of whisper trims a capture to where speech is and drops
one with no speech in it, since whisper hallucinates on silence. Which gate
runs depends on whether the Silero model at `asr.vad_model_path` exists.

`vad_threshold` is the Silero speech probability, between 0 and 1, at or
above which a frame counts as speech. 0.5 is the model's own default; raise
it if breathing or keyboard noise gets through, lower it if quiet speech
is cut.

`speech_threshold` is an RMS level between 0 and 1 for the energy gate,
which runs only without the VAD model. Frames above it count as speech.
Raise it in a noisy room, lower it for a quiet microphone.

`min_utterance_ms` drops captures shorter than this after trimming, which
catches accidental taps. `max_utterance_ms` caps what goes to whisper.
`max_hold_ms` is different: a chord held longer than this is treated as
released, the capture is finished and processed, and a lost key-release
event cannot record for ever. `min_utterance_ms` must not exceed
`max_utterance_ms`.

`sample_rate` and `end_silence_ms` come from older versions. They still
parse, do nothing, and produce a warning in the log.

## `[asr]`

```toml
[asr]
model_path = "~/.local/share/parla/models/ggml-large-v3-turbo.bin"
vad_model_path = "~/.local/share/parla/models/ggml-silero-v5.1.2.bin"
language = "en"
threads = 4
hallucination_blocklist = ["thank you", "thanks for watching", "the end", "subtitle"]
```

`model_path` is any whisper.cpp ggml model. `large-v3-turbo` is the default
because it returns in a few hundred milliseconds on a desktop GPU and is
accurate enough that cleanup has little to fix. `language` is a whisper
language code, or `auto` to detect per utterance. Detection costs a little
time and occasionally guesses wrong on short utterances, so set the code
when you dictate in one language.

`vad_model_path` is whisper.cpp's Silero VAD model, `ggml-silero-v5.1.2.bin`
from `scripts/fetch-model.sh vad`, under a megabyte and run on the CPU.
When the file is missing the daemon starts anyway with the RMS energy gate
in its place and says so in the log; `parlad --check` shows which gate is
active.

`initial_prompt` (absent by default) is text whisper sees before every
utterance, which biases it toward that vocabulary. The dictionary's words are
appended to it automatically, so most people never set this.

`hallucination_blocklist` names transcripts whisper produces from silence
or noise. A transcript that is only a listed phrase is dropped. A listed
phrase at the end of a real transcript is stripped, on a word boundary.
Matching is case-insensitive after whitespace normalisation.

## `[router]`

```toml
[router]
cues = true
notify_results = true
confirm_window_ms = 8000
```

`grammar_file` (absent by default) points at a TOML file of extra command
patterns. Its rules take priority over the built-in ones. The format is in
[commands.md](commands.md).

`cues` plays a short sound when capture starts, when it stops, and on an
error, through `pw-play` or `paplay`. The wav files are looked up next to
the binary under `../share/parla/cues` or `cues`, then under
`$XDG_DATA_HOME/parla/cues`. A missing player or file is logged and
ignored.

`notify_results` sends a desktop notification with the outcome of each
command: what was launched, what failed and why. A confirmation question is
always notified, whatever this says, since an action is waiting on the
answer. Dictation results go to the overlay and never to a notification.

`confirm_window_ms` is how long a "say yes" prompt stays answerable. A yes
after that is refused and the command has to be spoken again.

## `[local]`

```toml
[local]
model_path = "~/.local/share/parla/models/Qwen3-4B-Instruct-2507-Q4_K_M.gguf"
gpu_layers = 999
context_tokens = 8192
threads = 4
```

The GGUF instruct model that both the judged path and dictation cleanup
use. It is loaded once, on its own thread, only when at least one of
`judge.backend` and `flow.backend` is `local`.

`gpu_layers` is how many layers to offload. More than the model has means
all of them. `0` keeps it on the CPU, which works but makes cleanup take
seconds. `context_tokens` must be at least 512. A busy desktop's state plus
one question is a few thousand tokens, and a dictation's prompt is the
transcript plus the instructions, so 8192 leaves room for both. `threads`
are for whatever is not offloaded.

A 4B model at Q4_K_M takes about 2.5 GB of VRAM plus the context. Any GGUF
instruct model with a chat template works. Smaller models answer faster
and worse; the judge in particular wants a model that follows a closed
list of options.

## `[judge]`

```toml
[judge]
enabled = true
backend = "local"
timeout_ms = 4000
min_confidence = 0.35
act_unconfirmed_above = 0.65
dictation_threshold = 0.4
destructive_threshold = 0.8
send_window_titles = false

[judge.typesafe]
model = "jev-latest"
```

The judged path handles command-mode utterances the grammar does not match.
`enabled = false` makes those refusals instead: the grammar still works.

`backend` is `local` for the model under `[local]`, or `typesafe` for the
TypeSafe System One API. `timeout_ms` bounds one judgment. Past it the
command fails with an error rather than acting late on something the user
has already given up on.

The four thresholds are probabilities the policy compares against. They are
described with the decision procedure in [commands.md](commands.md). Each
is optional. A key that is absent takes the default for the backend in
use, so a config that only sets `backend = "typesafe"` gets the TypeSafe
numbers without naming them, and a key that is set wins for that key
alone. Each must be within 0 and 1, and `min_confidence` must not exceed
`act_unconfirmed_above` after the defaults are applied.

| key | `local` | `typesafe` |
| --- | --- | --- |
| `min_confidence` | 0.35 | 0.45 |
| `act_unconfirmed_above` | 0.65 | 0.75 |
| `dictation_threshold` | 0.4 | 0.5 |
| `destructive_threshold` | 0.8 | 0.6 |

The local numbers come from `parlad --calibrate` over the corpus in
`corpus/judge.toml`. The method and the results are in
[commands.md](commands.md). The TypeSafe numbers were picked by hand for
that API's calibrated outputs and have not been run against the corpus.
`--print-default-config` prints the four keys with the local values,
since `local` is the default backend, and `--check` prints the values in
force.

`send_window_titles` only affects the `typesafe` backend. By default the
request names each open window by its application class and an index, and
parlad maps the chosen index back to the window here. Titles carry document
names, URLs and chat subjects. `true` sends them. The local backend always
sees titles, since nothing leaves the machine.

`[judge.typesafe]` has `model` and `api_key`. Prefer exporting
`TYPESAFE_API_KEY` over writing the key into a file that gets copied around.
The key type redacts itself in logs and in `--print-default-config`.

## `[flow]`

```toml
[flow]
cleanup = true
backend = "local"
timeout_ms = 6000
max_tokens = 1024
history = true
edit_window_ms = 90000
context = true

[flow.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4.1-mini"
```

`cleanup` runs the rewrite model over each dictation. Off types what
whisper heard, after snippets and dictionary replacements, with spoken
"new line" and "new paragraph" turned into breaks. Voice edits ("make that
formal") need cleanup on, since they use the same model.

`backend` is `local` for the model under `[local]`, or `openai` for any
server that speaks the OpenAI chat completions API. `[flow.openai]` has
`base_url` (up to and excluding `/chat/completions`), `model`, and
`api_key`. `OPENAI_API_KEY` in the environment wins over the file. A local
llama-server or Ollama needs no key, and an absent key is sent as no
`Authorization` header rather than an error. Requests are sent with
temperature 0.

`timeout_ms` bounds one cleanup. Past it the raw transcript is typed, so a
slow model degrades to plain dictation rather than to nothing. `max_tokens`
caps what the model may generate for one dictation or edit.

`history` keeps `history.jsonl`. Off means the UI's history and statistics
pages stay empty and nothing about what was said is written to disk.

`edit_window_ms` is how long after a dictation "scratch that" and "make
that shorter" still refer to it. The reference also dies when focus moves
to another window.

`context` reads the focused text field over the accessibility bus when the
dictation hotkey goes down and tells the cleanup model what is before the
cursor, so the dictation continues it in the same language, register and
capitalisation, and gets a leading space when it needs one. It needs
`desktopd.a11y`. Nothing is read from a password field, and profiles with
the code tone get the leading-space rule only. With the OpenAI backend the
text before the cursor is sent with the transcript. The details are in
[dictation.md](dictation.md).

## `[desktopd]`

```toml
[desktopd]
terminal = "konsole"
terminal_run_args = ["-e"]
claude_tmux_session = "claude-main"
claude_command = "claude"
injectors = ["eis", "ydotool"]
focus_if_running = true
a11y = true
```

`terminal` and `terminal_run_args` are what "open terminal" runs and how it
is told to execute a command line. `claude_tmux_session` and
`claude_command` are the tmux session name and the command the Claude Code
intents drive.

`injectors` is the preference order for typing text and sending key
chords. `eis` is KWin's EIS interface over libei, which needs no daemon and
works on Plasma 6 Wayland. `ydotool` talks to a running `ydotoold` over its
socket; `ydotool_socket` (absent by default) overrides where that socket
is. The first injector whose probe succeeds at startup is used for the
daemon's lifetime, and `--check` reports which one that is.

`focus_if_running` makes a launch of an application that already has a
window focus that window instead of starting a second copy.

`a11y` connects to the session's accessibility bus at startup and sets
`org.a11y.Status.IsEnabled`, the flag that makes Qt, GTK, Firefox and
Chromium expose their text fields, when it is not already set. A flag
parlad set is cleared again at shutdown. The connection is what
`flow.context` and verified "scratch that" read from; off, cleanup does not
see the screen and edits delete as many characters as were typed. A bus
that cannot be reached is logged and the daemon runs without it.
