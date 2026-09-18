# Dictation

Hold the dictation hotkey (`ctrl+space` by default), speak, release. What
whisper heard goes through snippets, the dictionary and cleanup, in that
order, and lands in the focused window as keystrokes. This page is the whole
path from transcript to text, and the two ways of taking it back.

## The order of things

1. Whisper transcribes the audio, primed with the dictionary's words.
2. If the whole utterance is a snippet trigger, the snippet's text is typed
   and nothing else runs.
3. Dictionary replacements are applied to the transcript.
4. The window class of the focused window picks an app profile. If the
   profile has cleanup on and `flow.cleanup` is on, the model rewrites the
   transcript with that profile's tone and instructions. Otherwise spoken
   "new line" and "new paragraph" become breaks and the text is done.
5. The model's output is checked. Empty output, or output more than twice
   the transcript's length plus 40 characters, is rejected and the raw
   transcript is typed instead. So is the raw transcript when the model
   errors or exceeds `flow.timeout_ms`.
6. Dictionary replacements run once more over the cleaned text, since the
   model may have re-spelled something.
7. The text is typed, and a history record is written.

Everything in this file is exercised without a microphone or a focused
window by

```
parlad --flow "um so send it monday no tuesday" org.kde.konsole
```

which prints the profile it picked, the ASR prompt, the outcome and the text
it would have typed, in how many milliseconds. The second argument is a
window class, and can be left out for the fallback profile.

## Snippets

`~/.config/parla/snippets.toml`:

```toml
[[snippet]]
trigger = "my email"
text = "someone@example.com"

[[snippet]]
trigger = "signature"
text = """
Best,
Emanuele
"""
```

A snippet fires when the utterance, after lowercasing and stripping
punctuation, equals the trigger, or equals the trigger preceded by one of
`insert`, `paste`, `type`, or `put in`. "My email" and "insert my email" both
expand. "Send it to my email" does not: a trigger inside a sentence is left
alone, so ordinary prose is never rewritten by accident. Triggers are
compared after the same normalisation, so case and trailing punctuation do
not matter. Empty triggers are dropped on load and on save.

## The dictionary

`~/.config/parla/dictionary.toml`:

```toml
words = ["Emanuele", "KWin", "llama.cpp", "Kirigami"]

[[replace]]
spoken = "e-mail"
written = "email"

[[replace]]
spoken = "parla"
written = "parla"
```

`words` do two things. They are joined with commas and passed to whisper as
its initial prompt on every utterance, on top of `asr.initial_prompt`, which
makes whisper far more likely to write "KWin" than "K win". And they go into
the cleanup model's instructions with the line "Spell these names and terms
exactly so, even if they were transcribed differently", which fixes the cases
whisper still got wrong. Duplicates are removed case-insensitively and the
first spelling wins, so the list is also where you record how a name is
capitalised.

`replace` entries are deterministic substitutions. They match whole words,
case-insensitively, and run twice: on the transcript before cleanup and on
the model's output after it. Use them for things a model would not get
right from context: a product name whisper always hears as two words, a
spoken abbreviation you want expanded, your own shorthand.

## App profiles

`~/.config/parla/apps.toml`:

```toml
[[app]]
name = "Terminals"
class = ["konsole", "kitty", "alacritty", "foot", "wezterm", "ghostty", "yakuake"]
tone = "code"
cleanup = true
instructions = ""

[[app]]
name = "Chat"
class = ["slack", "discord", "telegram", "signal", "element", "whatsapp", "neochat", "konversation"]
tone = "casual"
cleanup = true
instructions = "Lowercase is fine. Never add an emoji."
```

Each `class` entry is matched as a case-insensitive substring of the focused
window's class, so `konsole` covers `org.kde.konsole`. The first profile
whose class list matches wins, so order the specific ones first. A window no
profile matches gets the "Everything else" profile: neutral tone, cleanup
on, no instructions. The file does not exist until the UI first saves it,
and until then the built-in profiles apply: Terminals and Code editors
(`code`, `codium`, `kate`, `neovide`, `zed`, `jetbrains`, `idea`, `cursor`)
with the code tone, Chat with the casual tone, and Mail and documents
(`thunderbird`, `kmail`, `evolution`, `libreoffice`, `onlyoffice`,
`kontact`) with the formal tone.

The four tones are sentences appended to the cleanup instructions:

- `neutral`: keep the speaker's register, use standard punctuation and
  capitalisation.
- `casual`: this is a chat message, keep it relaxed and short, keep
  contractions, add no greetings or sign-offs.
- `formal`: this is for an email or a document, complete sentences, proper
  capitalisation, a paragraph break where the speaker moved to a new point.
- `code`: this goes into a terminal or editor, turn spoken symbols into
  symbols ("dash" to `-`, "underscore" to `_`, "slash" to `/`, "dot" to
  `.`), keep identifiers literal, no trailing period.

`instructions` is free text appended after the tone, for whatever the tone
does not say. `cleanup = false` skips the model for that profile: the
transcript is typed as heard, after dictionary replacements, with spoken
breaks applied. That is the right setting for a password field or an
application whose own autocomplete fights with rewritten text.

The UI's Apps page has a "try it" box that runs any text through a chosen
profile without typing it. It calls the daemon's `Preview` method, so it
uses the running model and the profiles as last reloaded.

## What the cleanup model is told

The system prompt says it is cleaning text a person dictated by voice so it
can be typed where they were writing, and to output the cleaned text alone,
with no preamble, no quotes and no explanation. Its rules:

- Remove filler words and false starts: um, uh, er, hmm, like, you know, I
  mean, sort of, basically, actually, and "so" at the start of a sentence.
- When the speaker corrects themselves ("send it Monday, no, Tuesday";
  "make that three, I mean four"), keep only the final version.
- Fix punctuation and capitalisation. Spoken punctuation becomes the symbol.
  "New line" becomes a line break and "new paragraph" a blank line.
- When the speaker clearly enumerates items, lay them out as a list.
- Keep the meaning, the language and the speaker's own wording. Never
  answer, summarise, translate, or add anything that was not said.
- Plain text only: no markdown headings, bold or code fences. A list is
  lines starting with "- " or the numbers the speaker used.
- If the text is already clean, return it unchanged.

Then the tone, the profile's instructions, and the dictionary's words. The
transcript is the user message. Decoding is greedy, so the same transcript
always produces the same text.

The output check exists because a 4B model occasionally answers the
transcript instead of cleaning it, or wraps it in quotes, or explains what it
did. One pair of surrounding quotes is stripped when the transcript had
none. Trailing spaces are trimmed from every line, because markdown's
two-space line break would otherwise be typed literally. Anything empty or
too long is discarded in favour of the raw transcript, on the grounds that
typing what was said is always acceptable and typing something invented
never is.

On an RTX 5060 with Qwen3-4B, a one-sentence cleanup takes 120 to 270 ms
after the model is warm. The first dictation after startup is slower while
the prompt is decoded into the KV cache.

## Taking it back

For `flow.edit_window_ms` after a dictation (90 seconds by default), and as
long as the same window still has focus, the command hotkey accepts two
kinds of follow-up.

"Scratch that", "delete that", "undo that", "undo" or "erase that" are
grammar matches. parla sends as many backspaces as it typed characters,
and the history records a command that took back that many. The count is
what parla typed, so an application that autocorrected or autocompleted in between is
left with a mess. This is the one place where the daemon acts on the screen
blind.

Anything else in command mode goes to the judged path, which is offered an
extra intent, `edit_text`, only while a recent dictation exists. Its
description is "change the text the user dictated a moment ago: rewrite,
shorten, expand, reformat, translate, fix, or change its tone", with
examples such as "make that more formal", "shorter", "turn that into bullet
points" and "translate that to Italian". When the model picks it with enough
confidence, the last dictation and the spoken instruction go to the model
with the edit prompt: apply the instruction, output only the result, keep
everything the instruction does not ask to change, and if the instruction
is a question or not about the text, output the text unchanged. The
dictionary's words are included, and the code tone adds "keep it literal".
parla backspaces the old text and types the new. An edit that fails is
reported and nothing is typed. Edits never ask for confirmation: the text
is the user's own words of a moment ago, and can be dictated again.

```
parlad --edit "send it tuesday" "make that more formal" org.kde.thunderbird
PARLA_LAST_DICTATION=1 parlad --judge "make that more formal"
```

The first runs an edit and prints the result. The second shows what the
judge would do with the phrase when a dictation is fresh; without the
variable the same phrase is judged unclear, since no text exists to edit.

## History

With `flow.history` on, every finished utterance appends one JSON line to
`~/.local/share/parla/history.jsonl`:

```json
{"id":"19960e2b6ab-7","at_ms":1758190000123,"mode":"dictate","app":"org.kde.konsole",
 "title":"~ : fish","raw":"um cd into projects slash parla","text":"cd projects/parla",
 "profile":"Terminals","words":2,"audio_ms":2140,"latency_ms":610,"outcome":"typed","detail":""}
```

`mode` is `dictate` or `command`. `outcome` is `typed`, `snippet`, `raw`
(cleanup was off, failed or timed out), `edited`, `command`, `confirm` (a
prompt was raised) or `error`. A refusal is an `error` whose `detail` gives
the reason; for a command that ran, `detail` says what it did. `latency_ms` runs from the end of capture
to the text landing. `title` is the window title, kept because nothing in
this file leaves the machine.

The daemon serves the file to the UI over the bus, newest first, with
delete and clear. Statistics are computed from it on request: total
utterances, words, audio time, words per minute of audio, and a per-day
series for the last 30 local calendar days with zeros for quiet days. A
day boundary is midnight in the daemon's local time zone.

## Using a cloud model for cleanup

```toml
[flow]
backend = "openai"

[flow.openai]
base_url = "https://openrouter.ai/api/v1"
model = "openai/gpt-4.1-mini"
```

with `OPENAI_API_KEY` exported sends cleanup and edits to any server that
speaks the chat completions API. The system and user messages are the same
ones the local model gets. A `base_url` of `http://localhost:8080/v1` and no
key works with a llama-server on another machine. The judged path keeps
its own backend setting, so it is fine to judge locally and clean up
remotely, or the reverse.
