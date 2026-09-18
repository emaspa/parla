# Commands

Hold the command hotkey (`ctrl+shift+space` by default), say what you want
done, release. The transcript goes to the router, which tries a grammar of
literal patterns first and a model second, and every result passes one
policy that decides whether to act, ask, or refuse.

```
parlad --judge "bring the file manager to the front"
```

runs one utterance through both paths against the live desktop and prints
what would happen, without doing it. It is the way to see which path an
utterance takes, what the model answered, and what the policy made of it.
With `RUST_LOG=parlad=debug` it also prints every option's probability.

## The grammar

The grammar matches word sequences. The utterance is lowercased, its
punctuation stripped (inner apostrophes survive), and its whitespace
collapsed. Rules are tried in order and the first match wins. A rule is a
literal prefix, optionally ending in one capture slot that takes the rest of
the utterance. Some slots have a word cap: "open my email and find the
message from alan about the invoice" is eleven words after "open", so it
does not become a bogus application name and falls through to the model
instead.

The built-in rules, in priority order:

| pattern | intent | notes |
| --- | --- | --- |
| `yes`, `yes please`, `yeah`, `yep`, `confirm`, `do it`, `go ahead` | confirm | only meaningful while a prompt is up |
| `no`, `nope`, `cancel`, `stop`, `never mind`, `nevermind` | deny | |
| `scratch that`, `delete that`, `undo that`, `undo`, `erase that` | scratch_that | needs a recent dictation; checks the field still ends with it when the field can be read |
| `open terminal`, `open konsole` | open_terminal | |
| `start claude with {model}`, `start claude code with {model}`, `start claude code`, `start claude` | start_claude | |
| `claude model {model}`, `switch claude to {model}`, `switch claude model to {model}`, `set claude model to {model}` | claude_model | a leading "claude" in the name is stripped |
| `tell claude code {text}`, `tell claude {text}`, `ask claude {text}` | claude_tell | text is passed as spoken |
| `what did claude say`, `read claude`, `claude status` | claude_read | |
| `next desktop`, `previous desktop` | virtual_desktop_rel | |
| `desktop {n}`, `go to desktop {n}`, `switch to desktop {n}`, `virtual desktop {n}` | virtual_desktop | "two" and "2" both work |
| `close window`, `close the window`, `minimize window`, `minimize the window`, `maximize window`, `maximize the window` | close/minimize/maximize the focused window | |
| `close {query}`, `minimize {query}`, `maximize {query}` | same, on a window matched by title or class | up to 6 words |
| `focus {query}`, `switch to {query}` | focus_window | up to 6 words |
| `launch {query}`, `open {query}`, `start {query}` | launch_app | up to 6 words |
| `search for {query}`, `search {query}`, `find {query}` | krunner | up to 8 words |
| `notify {text}` | notify | |
| `press {chord}`, `key {chord}` | key | up to 3 words, e.g. "press control s" |

Window queries are fuzzy-matched against the titles and classes of the open
windows. Application queries are fuzzy-matched against the `.desktop` index
of installed applications.

Extra rules come from the file named by `router.grammar_file` and take
priority over the built-in ones:

```toml
[[rule]]
pattern = "take a screenshot"
intent = "run_shortcut"
args = { component = "org_kde_spectacle_desktop", action = "ActiveWindowScreenShot" }

[[rule]]
pattern = "save"
intent = "key"
args = { chord = "ctrl+s" }

[[rule]]
pattern = "look up {query}"
intent = "krunner"
max_capture_words = 8
```

`intent` is one of the names in the table plus `run_shortcut`, which fires
any existing KDE global shortcut by component and action, the same way
System Settings lists them. `args` supplies fixed arguments; a capture slot
supplies the rest under its own name. A rule with a token after its capture
slot is skipped with a warning.

## The judged path

What the grammar rejects goes to a model, unless `judge.enabled` is false,
in which case it is refused. The judge sends one request that carries the
utterance plus the state code already knows, and asks every question at
once:

- `intent`: which single action was asked for, from a closed list:
  `show_app`, `close_window`, `minimize_window`, `maximize_window`,
  `virtual_desktop`, `virtual_desktop_rel`, `open_terminal`, `start_claude`,
  `claude_model`, `claude_tell`, `claude_read`, `krunner`, `notify`, `key`,
  and `edit_text` while a dictation is fresh.
- `is_dictation`: is the utterance prose the user wanted typed, rather than
  an instruction? A yes means the user held the wrong hotkey.
- `is_destructive`: could carrying this out destroy unsaved work or be hard
  to undo? The model is told to judge the actual target in the window list,
  not the verb alone.
- `target`: which installed application or open window was meant. The
  candidates are the real ones, so the model cannot name something that is
  not there.
- `desktop_number` and `desktop_direction`: which desktop, when the intent
  is a desktop switch. On a one-desktop machine there is no number to
  choose.
- `claude_model`: opus, sonnet, haiku, or none named.
- `payload`: for a message to pass on verbatim, which trailing span of the
  utterance is the message. For "tell claude to fix the failing test" the
  candidates are "to fix the failing test", "fix the failing test", and so
  on, and the model picks where the command wrapper ends. The text reaches
  Claude Code as spoken, never rewritten.

`show_app` covers both launching and focusing. Whether the application
already has a window is an observed fact, so code decides that and the model
is not asked. When it was asked, in an earlier version, its probability
split between two spellings of one wish, and "bring the file manager to the
front" scored 0.44; letting code read the window list moved it to 0.78.

The state is JSON: the installed application names, the open windows with
their class and an index, the desktop count and current desktop, and
whether text was dictated moments ago. The local backend also gets window
titles. The TypeSafe backend gets them only with
`judge.send_window_titles = true`; otherwise the model picks an index and
parlad maps it back to the window here.

The judge remembers which window id or `.desktop` entry each candidate came
from, and that id is what gets executed. The title is never matched a second
time, so two windows with the same caption cannot swap places between
judging and acting.

### Scoring with the local model

The local backend never generates text for the judge. Every question has a
closed set of answers, so each option is scored: the probability the model
assigns to replying with exactly that option's key and then ending its
turn, normalised over the set. `a:1` and `a:12` are separate outcomes, not
one a prefix of the other. A yes/no question is the two-option set.

The desktop state is rendered once per utterance and shared through the KV
cache across the questions, and the options of one question are scored in
one batch as parallel sequences forked from the prompt. On an RTX 5060 an
utterance against a 12-window desktop took 700 ms and about 2000 prompt
tokens.

A 4B model's probabilities are peaky. Most answers come back at 1.00 or
0.00, and a wrong answer is as confident as a right one, so the thresholds
below separate little with this backend. "Calibrating the thresholds"
has the measurements.

### The TypeSafe backend

`judge.backend = "typesafe"` sends the same state and questions to the
TypeSafe System One API at `https://api.typesafe.ai/v1/systemone`, which
answers every question independently with calibrated probabilities. Export
`TYPESAFE_API_KEY`. Transient failures are retried with backoff under the
one `judge.timeout_ms` deadline.

Measured on a 123-application, 12-window desktop:

| utterance | result | confidence |
| --- | --- | --- |
| `could you bring up firefox for me` | launch Firefox | 0.93 |
| `shut down the browser` | close "Google Chrome" | 0.90, asks first |
| `I need a terminal` | focus the Konsole window | 0.89 |
| `The quarterly report shows a modest increase.` | dictation, not a command | refused |
| `what's the weather like tomorrow` | unclear | refused |

## The policy

Both paths end in one decision. For a grammar match the model was never
asked anything, so only the intent's own rule applies: `close_window`,
`run_shortcut`, `key` and `claude_tell` always ask first, everything else
acts. Closing can discard unsaved state, a shortcut and a key chord can do
anything, and a message to Claude Code makes an agent act on it.

The four thresholds are per backend. A key left out of `[judge]` takes
the backend's default: for `local` 0.35, 0.65, 0.4 and 0.8; for `typesafe`
0.45, 0.75, 0.5 and 0.6. The local numbers come from the calibration run
below.

For a judged intent, the checks run in this order and the first that
applies wins:

1. `is_dictation` at or above `judge.dictation_threshold`: refuse, "that
   sounded like dictation, not a command".
2. Confidence below `judge.min_confidence`: refuse. Confidence is the
   weakest link across the intent and each argument the intent needed, so a
   sure intent with an unsure target is an unsure command.
3. An intent that always confirms: ask.
4. No `is_destructive` answer: ask. A missing safety answer is never taken
   as safe.
5. `is_destructive` at or above `judge.destructive_threshold`: ask.
6. No `is_dictation` answer: ask.
7. Confidence below `judge.act_unconfirmed_above`: ask.
8. Otherwise act.

Being confident that parla understood a destructive request is not
permission to carry it out, which is why step 5 comes before step 7.

## Calibrating the thresholds

```
parlad --calibrate corpus/judge.toml
```

runs every case in a corpus through the judge and prints how the model
did and which thresholds would score best. The corpus is a TOML file with
a `[state]` block describing a synthetic desktop (installed applications,
open windows, desktop count, current desktop, whether Claude Code runs)
and a list of `[[case]]` entries, each an utterance with what the judge
ought to make of it: an intent name, or `dictation` for prose, or
`unclear` for what should be refused, plus the target, desktop number,
direction, payload or model the intent must carry. The comment at the top
of `corpus/judge.toml` lists every field. The desktop is the corpus's, not
the machine's. The tool builds no executor, opens no bus, and sends no
notification. It does load the model the config names, so a run takes the
same GPU memory as the daemon. Progress goes to stderr, the report to
stdout.

The tool reports and skips a case the grammar matches, since it never
reaches the judge. Every other case prints one line:

```
ok    show_app  show_app  target ok  conf 1.00 dict 0.00 destr 0.00 act  "bring up firefox"  ["GitHub - Mozilla Firefox"]
WRONG key       key                  conf 1.00 dict 0.00 destr 0.00 ask  "hit control s"     ["hit control s"]
```

In order: whether the verdict was right in every respect the case names;
the expected intent; the intent the judge built (`unclear` when none
could be); whether the target resolved to the one named; the weakest-link
confidence; the `is_dictation` and `is_destructive` probabilities; what
the policy would do under the thresholds in force (`act`, `ask`, or
`refuse` with its reason); the utterance; and the resolved target or
argument. A case that expects a refusal counts as right when the daemon
would refuse it for any reason. Whether the model recognised prose as
prose is a separate line in the summary.

The summary gives intent accuracy over the cases that expect an action,
target accuracy over the cases that name one, how many refusals happened
as expected, dictation precision and recall at the current threshold, a
histogram of the confidence behind right and wrong verdicts, and a sweep.
The sweep tries every `min_confidence` and `act_unconfirmed_above` pair
from 0.05 to 0.95 in steps of 0.05 and scores each as correct unconfirmed
acts, minus five per wrong unconfirmed act, minus one per correct verdict
that asked, minus two per correct verdict that was refused. The report
prints the best pair with the rows around it, and picks the middle of a
tie. It suggests `dictation_threshold` from the F1 of `is_dictation`
against the prose cases, and `destructive_threshold` from
`is_destructive` against the cases that say whether they are destructive,
taking the low end of a tie there because a missed prompt costs more than
a needless one.

Measured on 2026-09-18 with Qwen3-4B-Instruct-2507-Q4_K_M on an RTX 5060,
corpus of 135 cases, none matched by the grammar:

```
right in every respect: 99/135 (73%)
intent accuracy:     82/101 (81%) on cases that expect an action; with arguments 66/101
target accuracy:     30/42 (71%) on cases that name a target
refusals:            34/34 (100%) of cases that expect a refusal got one
prose:               14/14 refused, 6 of them recognised as dictation
dictation detection: precision 1.00 recall 0.43 at dictation_threshold 0.40
best score -12 at min_confidence 0.35, act_unconfirmed_above 0.65
at the best pair: acts right 49, acts WRONG 9, asks right 16, asks wrong 18, refuses right 0
```

The local defaults are that pair, 0.4 for dictation and 0.8 for
destructive. Two things the numbers say about this model. The confidence
carries little. 59 of 65 right verdicts and 22 of 27 wrong ones score
above 0.9, and any floor between 0.05 and 0.65 scores the same, so 0.35
is the middle of that range rather than a measured edge. And the errors
are systematic rather than uncertain. The model answers the payload
question with the whole utterance for every `notify`, `key` and `krunner`
case and for half of the `claude_tell` cases, so "hit control s" resolves
to the chord "hit control s". It picks "no target" for "fire up gimp" with
GIMP on the list. It calls eight of the fourteen prose cases not a
command rather than dictation. It calls starting Claude Code or opening a
shell destructive at 1.00. Those are for the questions and the model to
fix, and no threshold moves them. The corpus has not been run with the
TypeSafe backend.

To add a case, put it in `corpus/judge.toml` and rerun. The report says
whether the grammar took it first. Keep the cases the model gets wrong,
so a change to a question or a model can be measured against them.

## Confirmation

An action the policy wants confirmed becomes a spoken question with the
target resolved: "Close 'build — Konsole'? say yes", never "close the
terminal?". It is sent as a notification and shown on the overlay, and the
daemon's state is `waiting`.

The next command-mode utterance is matched against the confirm and deny
replies before anything else. A yes runs the command, a no drops it, and any
other command drops it too, since the user has moved on. Dictation in
between leaves the prompt alone.

A yes is honoured for `router.confirm_window_ms` (8 seconds by default),
and only if the target is still the window the prompt named: parla resolves
the query again and requires the same window id. A window that closed, or a
focus that moved, refuses instead of closing something else.

## What executes

parlad maps intents onto desktopd commands, and desktopd talks to the
desktop:

- Windows are listed, focused, closed, minimized and maximized through
  `kdotool`, which runs KWin scripts over D-Bus and works on Wayland and
  X11. Ids are KWin's UUIDs.
- Applications are launched from the `.desktop` index with `kioclient`,
  then `gtk-launch`, then `systemd-run --user`, whichever works first.
- Virtual desktops and KRunner go through KWin's own D-Bus interfaces.
- Global shortcuts fire through kglobalaccel's component objects, so any
  shortcut System Settings knows can be spoken without knowing its key.
- Claude Code is driven through a tmux session, never through keystrokes
  into a terminal window. "Tell claude" sends the text and Enter to the
  session; "read claude" returns the tail of its pane.
- Text and key chords are injected through KWin's EIS interface, or
  `ydotool` when that probe fails.
- Notifications go through the freedesktop notification interface.

`parla-probe` (built with desktopd) exercises each of these on its own:
`parla-probe windows`, `parla-probe resolve firefox`, `parla-probe
snapshot` for everything the judged path sees. `parla-probe type TEXT` and
`parla-probe key CHORD` act on the focused window, so use them with care.
