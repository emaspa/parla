# Dictation

Hold the dictation hotkey (`ctrl+space` by default), speak, release. What
whisper heard goes through snippets, the dictionary and cleanup, in that
order, and lands in the focused window as keystrokes. When the focused text
field can be read over the accessibility bus, the model also sees what is
before the cursor, and a word you fix by hand afterwards is noticed. This
page is the whole path from transcript to text, the two ways of taking it
back, and what parla learns from corrections.

## The order of things

1. Whisper transcribes the audio, primed with the dictionary's words.
2. If the whole utterance is a snippet trigger, the snippet's text is typed
   and nothing else runs.
3. Dictionary replacements are applied to the transcript.
4. The window class of the focused window picks an app profile. If the
   profile has cleanup on and `flow.cleanup` is on, the model rewrites the
   transcript with that profile's tone and instructions. With
   `flow.context` on, and a text field that was readable over AT-SPI when
   the hotkey went down, the prompt also carries the last 300 characters
   before the cursor; see [What the model sees of the
   screen](#what-the-model-sees-of-the-screen). Otherwise spoken "new
   line" and "new paragraph" become breaks and the text is done.
5. The model's output is checked. Empty output, or output more than twice
   the transcript's length plus 40 characters, is rejected and the raw
   transcript is typed instead. So is the raw transcript when the model
   errors or exceeds `flow.timeout_ms`.
6. Dictionary replacements run once more over the cleaned text, since the
   model may have re-spelled something.
7. If the field was read, a space goes in front of the text when the
   character before the cursor is not whitespace, a line break, an opening
   bracket or a quote, and the text does not itself start with whitespace
   or punctuation. A space goes after it by the same rule turned around:
   when the text after the cursor starts with a letter or digit and the
   text does not end with whitespace, an opening bracket or a quote. With
   the code tone, no space follows any other symbol either, so a path
   stays one path. This holds with `flow.context` off too, and for a
   snippet.
8. The text is typed, and a history record is written.

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

## What the model sees of the screen

Toolkits expose their widgets on the session's accessibility bus (AT-SPI):
Qt, GTK, Firefox, Chromium and Electron all do, but only once
`org.a11y.Status.IsEnabled` is true on that bus, which is what a screen
reader sets. With `desktopd.a11y` on, parlad sets it at startup, follows
focus over the bus, and clears the flag again at shutdown if it was parlad
that set it.

When the dictation hotkey goes down, the focused text field is read in the
same background task that looks up the focused window: the application's
name, the field's role, the caret position, and up to 600 characters
before the caret and 200 after. Every call to the application is bounded
by 300 ms, so a hung application costs a dictation its context and
nothing else. A password field is recognised by its role and its text is
never read.

With `flow.context` on, the cleanup prompt then ends with a section like

```
The cursor is in an entry in Thunderbird. The text before the cursor ends
with: "Hi Alan,

thanks for the". Continue that text: match its language, register and
capitalisation; if it ends mid-sentence, do not start with a capital; do
not repeat any of it.
```

quoting the last 300 characters. An empty field is described as empty.
The section comes last in the prompt so the local model's KV cache keeps
the part that never changes. Profiles with the code tone skip it: a
terminal's screen is not prose to continue, and the spacing rule is all
that applies there. The output check in step 5 still runs, so a model
that answers the context instead of cleaning the transcript is caught by
the length limit.

```
PARLA_CONTEXT_BEFORE="Hi Alan, thanks for the" parlad --flow "um quick reply" org.kde.thunderbird
```

tries the prompt without a live field: the variable stands in for the text
before the cursor. `parla-probe context` shows what parlad would read from
the field that has focus right now, or "no focused text field".

With `flow.backend = "openai"` the 300 characters go to the server with
the transcript. That is one more reason to keep the local backend.

## Taking it back

For `flow.edit_window_ms` after a dictation (90 seconds by default), and as
long as the same window still has focus, the command hotkey accepts two
kinds of follow-up.

"Scratch that", "delete that", "undo that", "undo" or "erase that" are
grammar matches. Before deleting, parla reads the focused field over the
accessibility bus and checks that the text before the cursor still ends
with what it typed. If it does, that many backspaces are sent and the
result says "took back 42 characters (verified)". If an application
changed a little of it in the meantime (autocorrect, a capital letter,
a bracket completed), the closest suffix is found instead: every length
within 30% of the typed length is scored by Levenshtein similarity, and
the best one is deleted if it scores at least 0.8, reported as "took back
44 characters (verified; 42 were typed)". If nothing at the end of the
field resembles the dictation, nothing is deleted and the command is
refused with "the text has changed since it was dictated". When the field
cannot be read at all (no accessibility bridge in that application,
`desktopd.a11y` off, a password field), parla falls back to sending as
many backspaces as it typed characters, reported as "(unverified)".
Deletion is always keystrokes; the bus is only read.

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
parla takes back the old text with the same verification as "scratch
that" and types the new; a field that no longer ends with the dictation
refuses the edit. An edit that fails is reported and nothing is typed.
Edits never ask for confirmation: the text is the user's own words of a
moment ago, and can be dictated again.

```
parlad --edit "send it tuesday" "make that more formal" org.kde.thunderbird
PARLA_LAST_DICTATION=1 parlad --judge "make that more formal"
```

The first runs an edit and prints the result. The second shows what the
judge would do with the phrase when a dictation is fresh; without the
variable the same phrase is judged unclear, since no text exists to edit.

## Learning from corrections

A name whisper spells wrong gets fixed by hand, and the fix is the
dictionary entry that would have prevented it. When the focused field
could be read at capture start, parlad reads it again after the
dictation and compares.

What is compared is the region the dictation occupies: about 20
characters of what was already before the cursor, the text parla typed,
and about 20 characters of what followed. Each edge is moved to the
nearest whitespace, so the context begins and ends with whole words. The
context is there to anchor the comparison. `flow.learn_after_ms` after
typing (20 seconds by default), or when the next dictation starts if that
comes first, parlad reads the same character range back, with 40
characters of slack at the end for words added after it. It splits both
texts into words, aligns them on their longest common subsequence, and
where a run of words was replaced by a run of the same length, pairs the
words up in order. The first or last word of the read is left out of a
pair when it is the tail or head of the word it stands against: text
edited earlier in the field shifts the region, and the read then starts
or ends inside a word. A pair is a correction when

- both words are letters, apostrophes and hyphens only, so numbers, paths
  and symbols never count;
- they differ;
- and they are the same word ignoring case and diacritics ("parla" to
  "Parla", "Zurich" to "Zürich"), or their edit distance is at most 34%
  of the longer word's length ("Emanuel" to "Emanuele", "wisper" to
  "Wispr"). "Monday" to "Tuesday" is a different word, not a spelling.

Inserted and deleted words do not count. Neither does a dictation that
was scratched, rewritten by a voice edit, or typed into a field that
cannot be read any more, and a password field is never read. A pair the
dictionary already produces (the written form is one of the words, or a
replacement exists for the heard word) is skipped, and so is one you
dismissed. The check runs against the dictionary as it is on disk when
the file is written, so a suggestion the UI just accepted does not come
back.

What survives goes into `~/.local/share/parla/learned.toml`:

```toml
dismissed = ["monday->Mondays"]

[[suggestion]]
heard = "Emanuel"
written = "Emanuele"
count = 2
last_at_ms = 1758190000123
app = "kmail"
```

`count` grows each time the same correction is seen; `app` is the
application it was seen in last, as its toolkit names it. `dismissed`
holds the keys of pairs turned down, the heard word lowercased and the
written word as is, so a dismissed pair stays away however it is
capitalised next time. Every recorded pair is logged at info.

`flow.learn` picks what happens next. With `suggest`, the default, the
file is all that changes, and the UI's Dictionary page lists the pairs
with Accept and Dismiss. Accept puts the written form into `words` and
the pair into `replace`, saves the dictionary, and asks the daemon to
reload. With `auto`, the daemon moves a pair seen twice into the dictionary the
same way, reloads the files, and, with `router.notify_results` on, sends
a notification saying "Learned: Emanuel -> Emanuele". With `off`, nothing
is read back. Learning needs `desktopd.a11y`; it works with `flow.context`
off, since the field is read either way.

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
