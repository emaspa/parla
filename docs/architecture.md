# Architecture

parla is a Cargo workspace of five crates and three binaries. The daemon
does all the work. The UI watches it over the session bus. A library in the
middle holds the files both of them read.

| crate | binary | what it holds |
| --- | --- | --- |
| `parla-grammar` | | The `Intent` type and the literal-pattern grammar, including the confirm and deny replies. No I/O. |
| `desktopd` | `parla-probe` | The executor: windows, launching, the `.desktop` index, virtual desktops, shortcuts, tmux, notifications, text injection. Its own `Command` type; it does not depend on the grammar. |
| `parla-flow` | | Dictionary, snippets, app profiles, history: the TOML and JSONL formats, with load, save and the matching rules. |
| `parlad` | `parlad` | The daemon: hotkeys, capture, speech gate, whisper, router, policy, confirmation, cleanup, the judged path, the local model, the bus. |
| `parla-ui` | `parla-ui`, `parla-mockd` | The Kirigami UI and its stand-in daemon. Not in the default build set because it needs Qt. |

`desktopd` is one implementation with two callers in mind. parlad maps
intents onto its commands and calls it as a library. Its command vocabulary
is the shape an MCP server would expose, so an agent could drive the same
desktop later without a second implementation.

## One utterance, start to finish

```
kglobalaccel ── pressed ──▶ capture thread (cpal) ──▶ samples
                                                          │ released, or max_hold_ms
                                                          ▼
                                             speech gate: Silero VAD (or RMS), trim, min length
                                                          │
                                                          ▼
                                             whisper.cpp on the GPU (spawn_blocking)
                                                          │ transcript
                          ┌───────────────────────────────┴───────────────────────────┐
                    dictate mode                                                 command mode
                          │                                                           │
              snippets → dictionary → cleanup (local model or API)          confirm/deny replies?
                          │                                                  grammar → intent
                          ▼                                                  judged path → intent
                    type into the focused window                                       │
                                                                                   policy: act / ask / refuse
                                                                                       │
                                                                                   desktopd command
```

1. **Hotkey.** kglobalaccel is asked to register the two chords under the
   component `parla` and parlad subscribes to its pressed and released
   signals. Plasma delivers those regardless of which window has focus, and
   the release signal is what makes hold-to-talk work without reading input
   devices. A daemon whose signal stream closes is deaf, so that is fatal and
   a supervisor should restart it.
2. **Capture.** At the press, the focused window and the focused text
   field are looked up in parallel with opening the microphone, so the
   window that gets the text is the one that had focus when the user
   started speaking, not when the model finished, and the text around the
   cursor is what was there before anything was said. The cpal stream lives
   on its own thread because opening a PipeWire device can block and the
   stream handle is not `Send`. The callback downmixes to mono, resamples
   to 16 kHz, and computes an RMS level every 40 ms for the bus.
3. **Gate.** At the release, the capture is cut to where speech is, on the
   blocking thread whisper uses. With the Silero model at
   `asr.vad_model_path`, whisper.cpp's frame-level VAD returns speech
   segments at `audio.vad_threshold`, and the capture is cut from the first
   segment's start to the last segment's end, with a 30 ms pad at each end.
   Pauses inside the span are kept: whisper copes with them, and
   concatenating segments would move words in time. Without the model
   file, or if the VAD call fails on a capture, the RMS energy gate trims
   leading and trailing frames below `audio.speech_threshold` instead. A
   capture with no speech, or shorter than `audio.min_utterance_ms` after
   trimming, is dropped before whisper sees it, because whisper on silence
   produces "Thank you." with confidence.
4. **Transcription.** whisper.cpp with CUDA, run on a blocking thread, with
   the dictionary's words as its initial prompt. Known silence
   hallucinations are dropped from the result.
5. **Routing.** Dictation goes through the flow and is typed; the path is in
   [dictation.md](dictation.md). A command is checked against the confirm
   and deny replies first if a prompt is waiting, then the grammar, then the
   judged path; both end in the policy. That is [commands.md](commands.md).
6. **Reporting.** The outcome goes to the log, to a notification if the
   config says so, to the bus as `LastResult` and an `Utterance` signal, and
   to the history file. The bus state walks through `recording`,
   `transcribing`, `thinking`, `typing` and back to `idle`, or to `waiting`
   when a prompt is up.
7. **Learning.** When the field was readable, parlad reads the dictated
   region back `flow.learn_after_ms` later, or when the next dictation
   starts, and records a word replaced by another spelling of itself in
   `learned.toml`; see [dictation.md](dictation.md).

Only one capture runs at a time, but processing is a separate worker: a
press while the last utterance is still being transcribed starts a new
capture, and finished captures queue for the worker, up to eight deep. On
SIGTERM or Ctrl-C a running capture is dropped and the worker gets a few
seconds to finish what is queued before the daemon exits.

## Threads

The daemon is a tokio runtime with a few dedicated threads around it:

- The **cpal thread** owns the input stream and talks to the runtime over
  channels.
- **whisper** runs inside `spawn_blocking`, one transcription at a time.
- The **local model thread** ("parla-llm") owns the llama.cpp context. It
  takes jobs from a channel, either evaluate (score options for the judge)
  or generate (cleanup, edits), and answers over a oneshot. One thread means
  the judge and the cleanup never compete for the GPU, and one model copy
  in VRAM serves both.
- The **bus** is a zbus connection on the runtime, serving the interface
  and publishing state from a watch channel, so the main loop never waits
  on a client.
- A **lock watcher** follows `org.freedesktop.ScreenSaver`.
- The **a11y follower** is a task on a second zbus connection, to the
  accessibility bus. It keeps the most recently focused object that has a
  Text interface, from `object:state-changed:focused` and
  `object:text-caret-moved` events, and a capture reads that object's
  text with calls bounded to 300 ms each. At startup, and whenever the
  kept object is no longer focused, the active windows are walked for the
  focused object, at most 400 objects and 1.5 s.

## The local model

`LocalModel` wraps llama-cpp-2 with a CUDA build of llama.cpp. It loads the
GGUF once with `local.gpu_layers` offloaded and a context of
`local.context_tokens`.

For the judge, no token is ever sampled. Each question's options are scored
as parallel sequences forked from the shared prompt, and an option's score
is the product of its tokens' probabilities followed by the end-of-turn
token, normalised over the option set. The desktop state is rendered once
per utterance; on the next prompt the longest common token prefix with the
previous one is kept in the KV cache and only the rest is decoded.

For cleanup and edits, decoding is greedy: argmax at each step, stop at the
model's end-of-generation token or at `flow.max_tokens`. The prompt is the
chat template's system and user turns. Both paths carry a timeout; a job
that exceeds it is abandoned by the caller, and the thread finishes it and
moves on.

## What leaves the machine

Nothing, in the default configuration. Audio never leaves the capture
thread. The transcript goes to whisper and to the local model, both in
this process. History is a local file.

With `judge.backend = "typesafe"`, each judged command sends the utterance,
the installed application names, and each open window's class and index.
Titles go only with `judge.send_window_titles = true`. With
`flow.backend = "openai"`, each dictation sends the transcript, the
profile's instructions, the dictionary's words and, with `flow.context`
on, up to 300 characters of the text before the cursor in the focused
field. The two switches are independent.

## Safety rails

Voice is an unauthenticated input channel: anyone within earshot of the
microphone can speak a command. The rails are built around that.

- **Session lock.** While the screen is locked, KWin sends all input,
  physical and injected, to the lock surface, and a spoken command must not
  work either. The daemon watches the screensaver interface, drops
  captures that end while locked, refuses to start new ones, and shows
  `paused` on the bus.
- **Single instance.** An exclusive `flock` on
  `$XDG_STATE_HOME/parla/parlad.lock`. Two daemons would register the same
  hotkeys and both type every utterance.
- **Held key limit.** `audio.max_hold_ms` finishes a capture as if the key
  had come up, so a lost release event cannot leave the microphone open.
- **Confirmation on consequence.** Closing windows, firing shortcuts,
  sending key chords and messaging Claude Code always ask, from either
  path. A judged command that the model calls destructive asks whatever
  its confidence. A missing safety answer counts as unsafe.
- **No fuzzy re-resolution.** The window id chosen at judgment time is the
  one acted on, and a confirmation re-resolves the target and requires the
  same id.
- **Cleanup output is checked.** Empty or runaway model output is replaced
  by the raw transcript, so the model can fail to help but cannot type
  something the user did not say.
- **Deletions are checked.** "Scratch that" and a spoken edit read the
  focused field first and delete only what is still there; a field that no
  longer ends with the dictation is left alone. A password field is never
  read.
- **Secrets stay out of logs.** API keys are a type whose `Debug` and
  serialisation print `<redacted>`.

## Desktop integration

| what | how |
| --- | --- |
| hotkeys | kglobalaccel over D-Bus, component `parla` |
| typing and key chords | KWin's EIS interface over libei (the `reis` crate), falling back to `ydotoold`'s socket |
| windows | `kdotool`, which runs KWin scripts over D-Bus; ids are KWin UUIDs |
| focused window | the same, resolved at capture start |
| focused text | AT-SPI over the a11y bus (the `atspi-connection`, `atspi-proxies` and `atspi-common` crates), read at capture start and before a deletion |
| launching | `kioclient exec applications:<id>`, then `gtk-launch`, then `systemd-run --user` with the parsed Exec line |
| installed apps | a `.desktop` index over the XDG application directories with fuzzy lookup |
| virtual desktops, KRunner | KWin's `VirtualDesktopManager` and KRunner D-Bus interfaces |
| shortcuts | kglobalaccel component objects, any registered action by name |
| Claude Code | a tmux session, driven with `send-keys` and read with `capture-pane`; a terminal is only ever attached for viewing |
| notifications | `org.freedesktop.Notifications` |
| screenshots | KWin's ScreenShot2 interface, without a portal dialog (available, unused by the router) |
| session lock | `org.freedesktop.ScreenSaver` |
| audio cues | `pw-play` or `paplay` with the wav files in `crates/parlad/assets/cues` |

One session bus connection is shared across the crate; zbus connections
are cheap to clone and expensive to open. The accessibility bus is a
separate bus with its own connection, opened once at startup.

## Logging

`tracing` with an env filter. `RUST_LOG=parlad=debug` shows every option's
probability on the judged path, the cleanup timings and token counts, the
hallucinations dropped, and the hotkey registration replies. ggml's own
log lines from whisper and llama.cpp are routed through `tracing`
so they do not spray stderr and can be filtered like everything else.

## Testing

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo clippy -p parla-ui --all-targets -- -D warnings
```

The unit tests cover the grammar, the policy, the judge's assembly of
questions and reading of answers (with canned responses), the flow's
matching and output checks, the history file, the config, and the bus
interface end to end over a private session bus with a unique name. None
of them need a GPU, a microphone or a Plasma session. The things that do
have read-only CLI entry points instead: `parlad --check`, `--judge`,
`--flow`, `--edit`, `parla-probe snapshot` and `parla-probe context` all
run against the live desktop without registering hotkeys or typing
anything. The suffix matcher behind verified deletions, the leading-space
rule and the prompt's context section are unit tests; the a11y connection
itself is only exercised through the probe.
