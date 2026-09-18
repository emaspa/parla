# The session bus interface

parlad owns the well-known name `org.parla.Daemon` on the session bus and
serves one object, `/org/parla/Daemon`, with the interface
`org.parla.Daemon1`. The introspection XML with every comment is
`crates/parlad/dbus/org.parla.Daemon1.xml`; this page explains how to use
it. The UI is one client. Anything that speaks D-Bus is another: a Plasma
widget, a script, `busctl`.

The daemon owns the pipeline. The bus exposes what it is doing, lets a
client start and stop a capture as a hotkey would, and answers history
queries. The editable files (dictionary, snippets, app profiles) are
written by the client itself, which then calls `Reload`. The daemon runs
fine with no client connected, and with no session bus at all: the bus is
how the UI sees the daemon, not something dictation depends on.

## States

`State` is one of:

| state | meaning |
| --- | --- |
| `idle` | waiting for a hotkey |
| `recording` | capturing audio; `Mode` says `dictate` or `command` |
| `transcribing` | whisper is running |
| `thinking` | the cleanup model or the judge is running |
| `typing` | text is being injected |
| `waiting` | a confirmation prompt is up and wants a spoken yes or no |
| `paused` | the session is locked, or `Enabled` is false |

Every change emits `StateChanged(state, mode)` and the standard
`PropertiesChanged` for the `State` and `Mode` properties. `Mode` is `""`
outside a capture.

## Properties

| name | type | access | |
| --- | --- | --- | --- |
| `State` | s | read | see above |
| `Mode` | s | read | `dictate`, `command`, or `""` |
| `Enabled` | b | read/write | `false` ignores the hotkeys until set `true`; state shows `paused` |
| `LastResult` | s | read | "typed 12 words", "Launched Firefox", an error message; cleared at the next capture |
| `LastResultIsError` | b | read | whether `LastResult` is an error |
| `Version` | s | read | the daemon's version |
| `Judge` | s | read | "local Qwen3-4B-Instruct-2507-Q4_K_M", "typesafe jev-latest", or `""` when disabled |
| `Cleanup` | s | read | the same for the cleanup model, `""` when cleanup is off |
| `DictateHotkey` | s | read | the chord as configured, e.g. "ctrl+space" |
| `CommandHotkey` | s | read | |

## Methods

`Start(s mode)` begins a hands-free capture in `dictate` or `command` mode.
It runs until `Stop()` or `audio.max_hold_ms`, whichever comes first, and
`Stop()` processes it exactly as releasing the key would. `Cancel()` drops
the capture without processing it. `Start` errors when a capture is already
running or the session is locked.

`Reload()` re-reads `dictionary.toml`, `snippets.toml` and `apps.toml`. The
error names the file and the parse problem. `parla.toml` is not reloaded;
that needs a restart.

`History(u limit, u offset) -> s` returns the newest `limit` records after
skipping `offset`, newest first, as a JSON array. `DeleteHistory(s id) ->
b` says whether the record existed. `ClearHistory()` empties the file.
`Stats() -> s` returns the aggregates. Both JSON shapes are below. With
`flow.history = false` these return empty results.

`Paths() -> s` returns a JSON object of absolute paths: `config`,
`dictionary`, `snippets`, `apps`, `history`.

`Preview(s text, s app) -> s` runs the dictation cleanup on `text` as if it
had been dictated into a window of class `app` and returns the result
without typing anything. This is the UI's "try it" box, and a handy way to
test a profile's instructions from a shell.

## Signals

`StateChanged(s state, s mode)` on every transition.

`Level(d level)` while recording, about 25 times a second, a microphone
level from 0.0 to 1.0. It is the RMS of the last 40 ms in decibels, mapped
so that -50 dB is 0 and 0 dB is 1. Nothing is sent outside a capture.

`Utterance(s record)` when an utterance has finished, whatever the outcome,
with the stored history record as JSON. With `flow.history = false` nothing
is stored and the signal is not sent; `LastResult` still changes.

`Error(s message)` for a failure or a refusal the user should see.

`Confirm(s question)` when a confirmation prompt is raised, together with
`StateChanged("waiting", "")`.

## JSON shapes

A history record:

```json
{
  "id": "19960e2b6ab-7",
  "at_ms": 1758190000123,
  "mode": "dictate",
  "app": "org.kde.konsole",
  "title": "~ : fish",
  "raw": "um cd into projects slash parla",
  "text": "cd projects/parla",
  "profile": "Terminals",
  "words": 2,
  "audio_ms": 2140,
  "latency_ms": 610,
  "outcome": "typed",
  "detail": ""
}
```

The fields are explained under History in [dictation.md](dictation.md).

Stats:

```json
{
  "utterances": 412,
  "words": 6180,
  "audio_ms": 2011000,
  "words_per_minute": 184.4,
  "days": [
    { "date": "2026-08-20", "utterances": 0, "words": 0 },
    { "date": "2026-08-21", "utterances": 14, "words": 231 }
  ]
}
```

`days` holds the last 30 local calendar days, oldest first, with zeros for
days without dictation.

## From a shell

```
busctl --user get-property org.parla.Daemon /org/parla/Daemon org.parla.Daemon1 State
busctl --user call org.parla.Daemon /org/parla/Daemon org.parla.Daemon1 Start s dictate
busctl --user call org.parla.Daemon /org/parla/Daemon org.parla.Daemon1 Stop
busctl --user set-property org.parla.Daemon /org/parla/Daemon org.parla.Daemon1 Enabled b false
busctl --user call org.parla.Daemon /org/parla/Daemon org.parla.Daemon1 History uu 5 0
busctl --user call org.parla.Daemon /org/parla/Daemon org.parla.Daemon1 Preview ss "um so send it monday no tuesday" org.kde.konsole
busctl --user monitor org.parla.Daemon
```

The first `Start` call is a good way to bind dictation to something other
than a held key: a mouse button through a KWin shortcut, a foot pedal, a
Stream Deck.

## A stand-in daemon

`parla-mockd` owns the same name and implements the whole interface with
made-up state: every few seconds it pretends to record (with `Level` at 25
Hz), transcribe, think and type, then emits an `Utterance`; now and then it
raises a confirmation or fails. `Start`, `Stop` and `Cancel` work. History
and stats come from a fabricated set of records. It refuses to start while
a real parlad owns the name, so it cannot hijack a live session.

```
cargo build --release -p parla-ui
./target/release/parla-mockd &
./target/release/parla-ui
```

It exists so the UI, or any other client, can be developed on a machine
with no microphone and no models.
