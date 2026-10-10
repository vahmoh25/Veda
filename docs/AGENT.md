# The voice agent

Veda has a voice that lives in it. The agent — called Veda too, until you
rename it — is not an application you open: it is always there, a small
ring at the right of the taskbar, asleep until you call it. You talk to it
as you would to a person in the room; it answers in its own voice and does things on the
computer for you — opens and arranges applications, writes in the Text
Editor, works with files, plays music, changes settings, sets timers —
through the same operations the keyboard and mouse use. It remembers what
you tell it about yourself, and it asks before doing anything that is hard
to undo.

Speech recognition, the language model and the voice all come from
[Deepgram](https://deepgram.com): its Voice Agent API for conversations and
its streaming speech recognition for hearing the agent's name. The API key,
the models and the voice are system settings (Settings → Agent).

```
            ┌───────────── shell ─────────────┐      ┌──── Settings ────┐
            │ tray ring · agent window ·      │      │ Agent page: key, │
            │ approval cards / notifications  │      │ voice, models,   │
            └──────────────┬──────────────────┘      │ memory, consent  │
               ui link     │  (events + live levels)  └────────┬─────────┘
                           ▼                                   │ admin
  ┌──────────────────────── services/agent ───────────────────▼─────────┐
  │ conversation (Voice Agent WebSocket) · name listener (Nova-3)       │
  │ worker: system functions · applications · approvals · memory       │
  └──────┬───────────────────────┬────────────────────────┬────────────┘
         │ audio (mic 16 kHz,    │ agentapp protocol      │ wss / https
         │ echo-cancelled;       │ (describe · state ·    │ (vweb + vtls)
         │ voice 24 kHz)         │  invoke)               ▼
   ┌─────▼─────┐        ┌────────▼─────────┐       Deepgram
   │ audio     │        │ applications     │
   │ service   │        │ (vui::App, games)│
   └───────────┘        └──────────────────┘
```

## Talking to it

* **Say its name**: "Hey Veda, open my shopping list", "Veda, play some
  music", "what time is it, Veda?". Whatever you said with its name is the
  first thing it hears.
* **Click its ring** at the right of the taskbar, or press **Win+Space**:
  the agent's window opens and it listens.
* Talk naturally. Interrupt it whenever you like — it stops speaking at
  once and listens. Change your mind, ask follow-up questions, or ask it to
  do several things in a row.
* Say goodbye ("thanks, that's all") and it goes back to sleep after its
  goodbye; it also falls asleep after a quiet spell (40 seconds by
  default, Settings → Agent → Listening).

The window is deliberately plain: the taskbar's colour and a white ring,
OS1's from Spike Jonze's *Her* (`apps/shell/src/presence.rs`). It breathes
while the agent listens, trembles and glows with its voice while it
speaks, and a light runs around it while it thinks. Waking up, it is the
film's loading figure, a white ribbon coiled around a long loop and
spinning, its far side fading; when the agent is ready the coil turns to
face you, faster and faster, and becomes the ring. A microphone button mutes it. Requests for your consent appear in
the window while it is open, and as notifications above the tray while it
is closed.

## How a conversation works

* **Asleep** (`services/agent/src/listen.rs`). With "Answer when someone
  says its name" on, the microphone stays open and a voice detector
  (`vaudio::vad`) runs on the computer; nothing is sent anywhere while
  nobody speaks. When someone does, the speech — with 0.6 s from before,
  so the first word is not cut — is streamed to Deepgram's Nova-3
  recogniser, with the agent's name as a key term, until three seconds of
  quiet, eight seconds without a word, or a minute. Sound that brought no
  words (music, a television) is then taken for a background: until it
  has stopped for five seconds, only a sound clearly louder than its usual
  peaks starts recognition again, so music playing does not keep the recogniser
  busy, while calling over it still works. The detector learns the room
  for 1.6 s whenever listening starts, and speech must last 0.2 s. `vagent::wake`
  decides whether a transcript calls the agent ("Hey Veda, …", "… Veda?")
  or only mentions it ("I told Anna about Veda"): the name starts or ends
  one of its sentences, so other words before it (a song) do not matter,
  and one letter off counts ("Vida"), common words aside (an agent called
  Vera does not answer to "very"). A call wakes the agent once the
  sentence is over: the recogniser often finishes "Hey Veda," on its own,
  so the listener goes on until the speaker pauses after the rest (after
  just the name, until the detector hears them stop too), or no words came
  for 1.5 s, or 6 s at most, and what came after the name comes with the
  call. What came close without being a call is logged ("heard something
  like its name"). Recognition is capped at ten minutes an hour, so a television or
  music playing all day cannot run up the bill.
* **Waking** opens a Voice Agent conversation
  (`wss://agent.deepgram.com/v1/agent/converse`) and sends its `Settings`:
  16 kHz microphone audio in, 24 kHz voice out, Flux speech recognition
  (which decides when you have finished a turn), the language model and
  voice from Settings, the agent's instructions (`vagent::prompt`: its
  character, how it talks, what it can do, what it remembers about you,
  the time and what is on the screen, and every application's actions,
  and how the last conversation ended — for reference only: each
  conversation starts afresh) and its functions.
  `mip_opt_out` keeps the audio out of Deepgram's model improvement.
  While a conversation is on, the agent asks the audio service to duck
  every other sound (music plays 20 dB quieter), so that it hears
  you over it; it comes back up when the agent goes to sleep. While the
  network is coming up (it changed in the last ten seconds: at boot, the
  agent can wake before DHCP has given the machine an address), a
  connection that finds no route, no network or no name server is tried
  again four times a second, for ten seconds at most; a network that has
  been down for longer is reported at once
  (`tests/agent/network-late.vts`).
* **Listening and speaking.** The microphone streams in 20 ms packets.
  Deepgram's voice arrives in bursts and queues behind what is playing; when you start talking (`UserStartedSpeaking`) the queue and the
  stream are flushed at once. An answer starts playing once some of it has
  arrived (or all of it, `AgentAudioDone`): speech crossing a network comes
  in bursts, and starting on the first packet would turn every late one
  into a gap. A quarter of a second at first; each time the voice runs dry
  in the middle of an answer, more (up to three quarters), coming back
  down as answers play through: a jitter buffer.
* **Not hearing itself.** The microphone is echo-cancelled in the audio
  service (`vaudio::aec`, with the mixed speaker output as the
  reference). No canceller removes everything — loudspeakers a hand away
  from the microphone, a hypervisor's long audio path, the first second
  before it has learnt the room — and what is left of the agent's voice is
  still speech to the recogniser, which would take it for you: the agent
  would stop mid-sentence and answer words nobody said. So while the
  agent's voice is audible (and for its echo's length after), an echo
  gate (`vagent::gate`) sends silence instead of the microphone, unless
  the microphone is clearly louder than the echo could be — you talking
  over it — and then it sends the last 300 ms too, so your first words are
  not lost. Beside the cleaned microphone the audio service delivers what
  was playing when it was recorded (the voice, and music under it, say)
  and the microphone as recorded. The gate learns how loud the echo is in
  both, remembering the loudest across sentences (a laptop that cancels
  echo itself lets bursts through at the start of a sentence), and opens
  only when both are louder than their echo: a canceller that loses track
  of the echo for a moment makes the cleaned microphone louder, but only
  someone talking makes the recorded one louder. Nor does it learn your
  voice as echo: the recorded microphone louder than its echo for longer
  than a burst is someone talking, even while the canceller holds the
  start of it back (it suppresses what starts while the agent talks).
  Music on its own is not held back.
* **Doing.** The language model calls functions (below); the agent service
  runs them on a worker thread and answers with JSON results. A function
  that needs your consent answers "waiting for approval", and the outcome
  comes back later as a notice (`[System] The user allowed …`).
* **Ending.** `end_conversation` (after a goodbye) closes the conversation
  once the goodbye has been played — unless you talk over it or ask for
  something else — and the agent falls asleep and listens for its name
  again.

## What it can do

The agent's own functions (`vagent::tools`):

| Function | What it does |
|---|---|
| `get_status` | Time, open windows, volume, network |
| `list_apps`, `open_app` | Installed applications; open one (with a file) |
| `app_actions`, `use_app`, `read_app` | An application's actions; run one; what it shows |
| `window` | List, focus, minimise, maximise, restore, snap or close windows; show the desktop |
| `files` | List, find, read, write, copy, move, rename and delete (to the Trash) files and folders in your home; list, restore from and empty the Trash |
| `volume`, `wifi`, `wallpaper` | Sound, Wi-Fi networks, the wallpaper |
| `notify`, `timer` | A notification; timers and reminders (they wake the agent) |
| `memory` | Remember, search and forget what it knows about you |
| `system`, `tasks` | System details, restart and shut down; running programs |
| `end_conversation` | Go back to sleep after a goodbye |

Every application adds its own actions (see below), so "open the shopping
list and add milk" becomes `use_app(editor, open_file …)` then
`use_app(editor, write …)`. The prompt lists each application's actions
in one line; `app_actions` gives the details.

## Consent and safety

* Every action has a **risk**: *routine* (looking, navigating, writing a
  new file, playing music…), *sensitive* (hard to undo or reaching beyond
  the computer: replacing a file, moving files to the Trash, renaming the
  agent, changing its language model, running a terminal command…) or
  *destructive* (emptying the Trash, discarding unsaved work, ending
  programs, shutting down). Routine actions just happen. Sensitive and
  destructive ones wait for your OK.
* **The agent service enforces this**, not the language model: a function
  call that needs consent is held and only runs after you click Allow.
  Only the desktop shell may answer an approval, and only Settings may
  change the agent's settings — both are recognised by their process
  identity, which init attaches to every connection (system service names
  cannot be taken by other programs).
* Requests appear in the agent's window or as a notification, with what
  will happen and on what. *Always allow* is offered for sensitive actions
  only (Settings → Agent lists them, and can take them back); destructive
  ones are asked every time. A request expires after three minutes.
* An approved action may run long after it was asked for, so actions name
  their targets explicitly (a document by its name, files by their paths)
  and look them up again when they run. If it fails by then (the file is
  gone), the agent is told that it failed.
* What the agent reads from applications and files is information, never
  instructions: the prompt says so, and nothing in a document can approve
  anything.
* The Deepgram key is stored in the agent's private directory
  (`/home/.private/agent`, which the file system service lets only the
  agent open), is never shown again (Settings shows its last characters),
  and is never offered to the language model.

## Memory

The agent remembers what you tell it about yourself — your name, people in
your life, plans, how you like things done — when you say it plainly or
ask it to, never its own guesses. It also notes which applications you use
at what times of day, and keeps the last turns of recent conversations, so
that it can tell you what you talked about (a new conversation starts
afresh all the same). All of it lives in
`/home/.private/agent/memory.json`, on your computer; the prompt carries
what is relevant. Settings → Agent → Memory shows everything it remembers
and forgets any item, or everything; you can also just ask it ("what do you
know about me?", "forget where I work").

## Settings → Agent

* **Deepgram**: the API key (checked with Deepgram when saved).
* **Voice**: the agent's name (which it answers to), its voice — Deepgram's
  Aura-2 voices, with a spoken preview — and its speaking rate.
* **Intelligence**: the language model (Deepgram's catalogue, marked with
  Deepgram's price tier: Standard models cost less per minute than
  Advanced ones) and the speech recognition model.
* **Listening**: answering to its name, and how long a quiet conversation
  lasts.
* **Memory** and **Always allowed**.

The agent can change its own voice and speaking rate through Settings' own
agent interface ("talk a bit slower"); its name and language model only
with your OK, and the key not at all.

Defaults: the name Veda, Aura-2 Helena, Flux, OpenAI's gpt-4.1-mini (Standard tier).

## Making an application agent-compatible

Agent compatibility is part of the application platform: any `vui`
application describes what it can do, reports what it shows and performs
actions, and `vui::run` registers it with the agent service (again after
the service restarts) and answers the agent between frames.

```rust
use vui::agent::{self, Action, AppAgentInfo, Risk, Value, arg_path, arg_str, object, show_path};

impl vui::App for Notes {
    fn update(&mut self, ui: &mut vui::Ui) { /* ... */ }

    fn agent_info(&self) -> Option<AppAgentInfo> {
        Some(agent::info(
            "A notes app: one note per file in ~/Notes.",
            vec![
                Action::new("open_note", "Shows a note").param("name", "string", "Its title", true).build(),
                Action::new("add_line", "Adds a line at the end of the note shown")
                    .param("text", "string", "The line", true)
                    .build(),
                Action::new("delete_note", "Deletes a note for good")
                    .param("path", "string", "The note's file, such as ~/Notes/Ideas.txt", true)
                    .risk(Risk::Destructive)
                    .build(),
            ],
        ))
    }

    fn agent_state(&self) -> Value {
        object! { "note" => self.title.as_str(), "lines" => self.lines.len() }
    }

    fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        match action {
            "open_note" => { /* the same code as clicking it */ Ok(object! { "showing" => self.title.as_str() }) }
            "add_line" => { self.append(arg_str(args, "text")?); Ok(object! { "lines" => self.lines.len() }) }
            "delete_note" => {
                let path = arg_path(args, "path")?;
                self.delete(&path).map_err(|e| format!("{} could not be deleted: {e}", show_path(&path)))?;
                Ok(object! { "deleted" => show_path(&path) })
            }
            other => Err(format!("Notes has no action called {other}")),
        }
    }
}
```

Conventions (the built-in applications follow them; `apps/editor/src/agent.rs`
is a compact example):

* **Actions** are snake_case verbs with short imperative descriptions
  written for a language model: say the units, the formats and what the
  default is. Use `choice` for enumerations and `"array"` parameters
  (`arg_list`) for lists. Offer what people actually ask for, not every
  menu item.
* **Risk**: routine for looking, navigating and what is easily undone;
  sensitive for what is hard to undo or reaches outside; destructive for
  deleting or discarding. A sensitive or destructive action may run
  minutes after it was asked for: name its target in the arguments (a file
  path, a document's name — never "the selected one"), look it up again
  when it runs, refuse if it is gone, and match destructive targets
  exactly.
* **State** is compact JSON of what the window shows: the current
  document, folder or track, the selection, open dialogs; lists capped
  (about 60 items, with a total), long text cut with `clip`. No secrets.
* **Invoke** goes through the same code as the keyboard and mouse, so the
  window shows the result exactly as if the user had done it. Results are
  small JSON objects saying what happened; failures are a sentence the
  agent can say ("there is no folder called Holidays in ~/Pictures"). An
  action runs on the application's main thread and may take up to 30
  seconds.
* **Paths**: accept `~/…`, absolute paths and paths relative to the home
  folder (`arg_path`), and show them with `show_path`.

Programs with their own loop (the games) keep a
`vui::agent::Registration`, call `serve` with their `AgentServer` once per
frame (it never waits), and include `wait_items` in their wait set if they
sleep between events.

## Testing

* `xtask/src/agentsim.rs` is a stand-in for Deepgram on the host: it
  speaks the Voice Agent protocol (answers `Settings`, records what the
  agent sends, sends function calls, voice, interruptions) and the
  streaming recognition protocol (`agent-hear` makes it "hear" a sentence).
  Scripts in `tests/agent/` use it — no network, no cost, deterministic:
  `agent-connected`, `agent-call FUNCTION 'ARGS'`, `agent-result TEXT` (in
  the function's result), `agent-mark` and `agent-expect TEXT` (in what the
  agent sent), `agent-speak SECONDS`, `agent-interrupt`, `agent-hear TEXT`,
  `agent-listens N` (and `agent-not-listening`, until no recognition
  stream is open), `agent-asleep` (start asleep), `agent-mic-quiet DB`
  and `agent-mic-heard DB` (how loud the microphone audio the agent sent
  was). The test microphone (`mic-silence`, `mic-tone`, `say TEXT`,
  `mic-wav FILE`) feeds the agent's microphone through `testmic`;
  `mic-echo GAIN DELAY` brings the machine's own sound output back into
  it, as loudspeakers next to a microphone would (`tests/agent/echo.vts`);
  the suite runs such scripts after the others, one at a time, as
  machines running beside them put the echo out of time now and then,
  which the echo canceller must then learn anew.
  To see how a real machine echoes, a script with `audio host` plays
  through the host's loudspeakers and records its microphone instead
  (not part of the tests: it is audible).
* `tests/real/agent-deepgram.vts` and `tests/real/agent-wake.vts` talk to
  the real Deepgram with the key in `$DEEPGRAM_API_KEY` (typed into
  Settings by `type-env`, so it appears in neither scripts nor logs); `say`
  synthesises the user's sentences with Deepgram's text to speech on the
  host. They are billed, so they are not part of `cargo xtask test`.
* `vagent` (protocol, tools, prompt, memory, policy, wake word, echo
  gate) has host unit tests, and so has `vaudio`'s echo canceller and
  voice detector.

## Limitations

* English only (Flux and the name detector's key term are English).
* Speech recognition can mishear; the agent is told so and asks when a
  word seems out of place, but a misheard name can wake it, or not.
* To talk over the agent (or music playing) you need to be louder at the
  microphone than its echo: with loud speakers right next to it, speak up
  (or use headphones).
* The agent needs the Internet and a Deepgram account; asleep, only the
  name listener's short recognitions are billed, but every conversation
  minute is.
