# Agent mode

Press an agent's hotkey, speak an instruction, and whisrs sends it to a coding agent session
(Claude Code first) running in that agent's directory. When the turn ends, whisrs speaks a
short reply. The next press continues the same session, so the user can answer a question or
make the decision the agent asked for. Opt-in: with no `[[agents]]` entry nothing changes.

One PR covers every harness. Phase 1 is Claude Code and is tested by hand before phase 2
(OpenCode, Codex, GitHub Copilot CLI) starts.

## Config

```toml
[[agents]]
name = "jarvis"                  # required, unique, [a-z0-9_-]
hotkey = "Super+Shift+J"         # optional, same syntax as [hotkeys]
new_session_hotkey = "Super+Shift+N"   # optional: a voice turn in a fresh session
harness = "claude"               # claude (phase 1); opencode | codex | copilot (phase 2)
dir = "~/Projects/whisrs"        # required, ~ expanded, must exist
permissions = "auto"             # auto (default) | bypass | ask
command = "claude"               # optional binary path; default: the harness name on PATH
args = ["--remote-control", "jarvis"]   # optional, passed on the first turn of a session
reply_instruction = "..."        # optional, replaces the built-in voice instruction
```

- `permissions` is harness-neutral and mapped per driver. No warning for `bypass`: the user
  chose it. Claude: `auto` → `--permission-mode auto`, `bypass` → `--permission-mode
  bypassPermissions`, `ask` → `--permission-mode default`. `ask` passes the flag rather than
  none: without it a session starts in the user's `permissions.defaultMode`, which may be
  `auto`, and `ask` would not ask.
- `permissions`, `args` and `reply_instruction` are fixed when a session starts (a resume
  passes no flag, see below), so a change applies from the next `--new`.
- Validation warns (and skips the entry) on a missing/duplicate name, unknown harness or
  missing dir; a hotkey clash is reported like the `[[llm_commands]]` clashes. Agents with
  `[tts] enabled = false` get one warning: replies arrive as notifications only.
- `new_session_hotkey` has the syntax and clash checks of `hotkey` (it is one more binding
  source, so it also clashes with the agent's own `hotkey`), and starts the hotkey listener on
  its own.
- `command` exists because a systemd user unit often lacks `~/.local/bin` on PATH. A spawn
  failure names it.

## CLI and IPC

| CLI | IPC |
|---|---|
| `whisrs agent` (list: name, harness, idle/working/needs-you, bg id) | `{"cmd":"agent","action":"list"}` → `Response::Agents` |
| `whisrs agent <name>` (voice turn; same as the hotkey) | `{"cmd":"agent","action":"toggle","name":"…"}` |
| `whisrs agent <name> --text "…"` (typed turn) | `{"cmd":"agent","action":"send","name":"…","text":"…"}` |
| `whisrs agent <name> --new` (voice turn in a fresh session; same as `new_session_hotkey`) | `"action":"toggle"` + `"new":true` |
| `whisrs agent <name> --new --text "…"` (typed turn in a fresh session) | `"action":"send"` + `"new":true` |
| `whisrs agent <name> --reset` (forget the session, send nothing) | `"action":"reset"` |
| `whisrs agent <name> --stop` (stop the running turn, keep the session) | `"action":"stop"` |
| `whisrs speak --text "…"` / `--text -` (stdin) | `{"cmd":"speak-text","text":"…"}` |
| `whisrs agent-hook` (hidden; harness hook → daemon) | `{"cmd":"agent-event","payload":{…}}` |

- `speak-text` is its own variant, not a field on `speak`: an older daemon must answer it
  with an error, never read the selection instead.
- `agent-hook` reads the hook JSON on stdin, forwards it, and always exits 0 within two
  seconds, even when the daemon is down. It never blocks the harness.
- The CLI stays a thin client. All logic is in the daemon.
- The list shows the bg id (what `claude attach`, `claude logs` and `claude stop` take; the
  row also carries the short session id). An agent with no whisrs turn running is shown as
  `claude agents` sees its session, so a session working on its own reads `working`. One
  `claude agents --json --all` (it lists every session) answers for every such agent, with a
  five-second timeout, since the CLI waits on it.
- `--stop` with no whisrs turn running stops the session when it works on its own.

## Turn flow

1. **Toggle while Idle:** if this agent has a turn running, refuse with a short spoken and
   notified "Still working on the last one." (never queued twice while one is queued or
   playing). Otherwise record exactly like command mode's
   instruction recording (overlay, cues, both streaming and batch paths), with the agent as
   the sink instead of the LLM rewrite. No selection capture. Nothing to send (audio too
   short or silent, a prompt echo, an empty transcript) is spoken as "I didn't catch that.",
   and a transcription failure as the error's first sentence; both are notified per
   `[general] notify`, so they are heard with notifications off. Dictation's own wording is
   unchanged.
2. **Toggle while Recording for this agent:** stop and transcribe. Recording for anything
   else: refuse as busy, like command mode.
3. **Toggle while Speaking:** stop playback and start recording for this agent (barge-in, so
   answering the agent's question is one press). A press refused because this agent's turn
   runs stops only this agent's own reply or notice: it is then refused with a notification
   only (per `[general] notify`; no notice is queued in its place), so a press always silences
   its agent. Another agent's reply plays on (stopping it would lose it), and the refusal's
   notice is spoken after it, deduplicated as in 1. A selection read-aloud is not stopped for a
   refused press.
   - **Fresh session (`--new`, `new_session_hotkey`):** records exactly like a toggle; the
     recording remembers that its press asked for a fresh session
     (`AgentRecordingContext::new_session`), and a second press of either kind stops it. When
     the transcript is ready the session is forgotten (`reset`) and the turn starts a new one;
     nothing to send forgets nothing. Refused while any whisrs turn of the agent runs, at the
     press and again at the transcript. One that waits for you, which a plain press would
     answer, is refused with the needs-you notice it announced ("Waiting for your approval to
     use Bash.", spoken once per press burst like the busy notice) and "A plain press answers
     it; to start fresh, stop it first: whisrs agent <name> --stop"; `--new --text` and
     `--reset` say the same.
4. **Transcript ready:** start the turn on a spawned task. The state machine returns to Idle
   at once; dictation keeps working while the agent runs.
5. **Turn ends:** reply text → spoken text (below) → spoken when the daemon is Idle and no
   recording captures (a FIFO of pending replies, drained by watching the state channel like
   `hook_dispatch_loop` and the recording status), and shown as a notification when
   `[general] notify` is on. A meeting recording does not touch the dictation state, so the
   capture is checked on its own: a reply would play mid-call and land on the system track.
   Held replies are spoken once the recording stops. Replies are held from the moment a
   recording starts, before its devices open, and a reply or notice being synthesized or
   played when it starts is stopped and put back at the front of the FIFO, so it plays after
   the recording (a selection read-aloud is left alone). A recording start never waits on
   the dictation lock for that stop: when the lock is busy (a dictation start opening its
   mic), the stop is spawned and runs once the lock is free, while replies are still held.
   The stopped utterance is copied before the stop, since the stop wakes the reply queue,
   which clears what it was playing. A command-mode or `[[llm_commands]]` press holds them
   too, from its state check until its recording starts or it gives up, and a read-aloud
   press until its read-aloud started: each releases the dictation lock meanwhile (command
   mode and read-aloud to capture the selection), and the next queued reply would otherwise
   take the Idle state from it. Every hold is checked under the dictation lock where a reply
   starts (`speak_text`). A `--stop` with no whisrs turn running that stopped the session's
   own work clears the needs-you its permission prompt set. Errors are
   spoken too, as a short line
   (the message's first sentence, `spoken_error`; prefixed with the agent's name when several
   are configured), and notified per `[general] notify`, so they are heard with
   notifications off.
6. Several agents can run turns at once. Only one recording exists at a time.
7. **Replies the session produces on its own:** a `Stop` for one of our sessions that arrives
   with no whisrs turn running (a background command finished and Claude went on by itself,
   or the user typed in the session) is spoken like a reply, same FIFO and hold rules. Only a
   turn that ended on a poll arms a duplicate check: its own `Stop` can still arrive after
   it, so the next `Stop` with no turn running whose raw text equals that reply (whitespace
   collapsed) is not spoken; any other is a real follow-up. A permission-prompt
   `Notification` with no turn running marks the agent needs-you and speaks the notice, like
   one inside a turn. Before a new turn, `claude agents` is asked: a session `working` on its
   own is not `claude stop`-ped; the turn is refused with "It's still running a background
   task." (and with "Still working on the last one." while a whisrs turn runs), and the
   notification repeats the instruction, so it can be sent again. Measured on 2.1.301: after
   the turn's own reply the listed `state` is `done`, but `status` stays `busy` for as long as
   a background task runs, so between turns `busy` counts as working (a running turn still
   ends on `done`). A long-running background task (a dev server) refuses turns until it
   ends or the session is stopped: `whisrs agent <name> --stop` stops it and ends that task
   with it.
8. **Dictation and commands cut agent playback:** while an agent reply or notice plays, a
   dictation toggle, `command` or an `[[llm_commands]]` press stops it and goes ahead.
   A selection read-aloud (and `speak --text`) still refuses them, as before.
   `whisrs cancel` while an agent reply plays also drops the replies queued after it.

## Claude Code driver (verified on 2.1.301)

- **Environment:** inherit the daemon's environment minus an explicit denylist of what a
  parent Claude session sets (`PARENT_SESSION_VARS`): `CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`,
  `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ATTENDED`,
  `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`, `CLAUDE_CODE_SSE_PORT`,
  `CLAUDE_CODE_EXECPATH`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `CLAUDE_PROJECT_DIR`,
  `CLAUDE_JOB_DIR`, `AI_AGENT`, plus whisrs's own `WHISRS_*` keys. Everything else passes,
  Claude Code's own configuration included (`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN`,
  `CLAUDE_CODE_USE_*`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, ...). cwd = `dir`.
- **Its own scope under systemd:** the first `claude --bg` starts Claude Code's background
  service (`claude daemon run --origin transient`), which hosts every background session,
  the user's own included. Measured: started from a whisrsd in a unit with the default
  `KillMode=control-group`, the service and its sessions sat in the unit's cgroup and died
  with it on stop. So when whisrsd runs as a service (`INVOCATION_ID`, or a `.service`
  cgroup), `--bg` launches run under `systemd-run --user --scope --quiet --collect --`
  (environment, cwd and pipes kept); a missing `systemd-run`, or one that cannot reach the
  user manager, falls back to a direct spawn. `claude agents` and `claude stop` never start
  the service (measured) and run directly.
- **First turn:** `claude --bg --name whisrs-<agent> --permission-mode <m>
  --append-system-prompt <instruction> --settings <hooks json> [args…] -- "<text>"`.
  `--session-id` is ignored with `--bg` (measured on 2.1.301: "--bg manages the session id;
  ignoring --session-id"), so the session id is read from `claude agents --json --all` by
  the bg id that `--bg` prints (`backgrounded · <id>`). The bg id is reported and persisted
  the moment it is read, before that lookup, so `--stop` and a restart know it. The launch
  runs on a task of its own: a `--stop` that lands while it runs is recorded, and the launch
  stops the session it started. The appended prompt is recorded on the first request and
  reused on every resume (`--system-prompt-snapshot` defaults to on), so it is passed once.
- **Hooks json:** `Stop` and `Notification` → `<whisrs> agent-hook --socket <socket>`,
  short timeout. `<whisrs>` is the absolute path of the `whisrs` beside this `whisrsd`,
  else plain `whisrs`, found on the session's `PATH`; the socket is explicit because the
  session runs under Claude Code's background service, whose environment is not the
  daemon's. Passed with `--settings`, so only whisrs-started sessions have them; the user's
  own settings are untouched.
- **Next turn:** refused while the session is listed `working` (above). Otherwise, if the bg
  process is still alive, `claude stop <bg id>`, then wait until the session's row has no
  `pid` and the process it listed has exited (`/proc/<pid>`, a zombie counts as exited),
  checked every 250 ms (the process every 50 ms once the row lost its pid), plus 250 ms for
  Claude Code to settle it, capped at ten seconds. Every pid a listing shows (the next-turn
  check, the waits, the poll, `whisrs agent`, and a listing just before every `claude stop`)
  is remembered per bg id with its `/proc/<pid>/stat` start time until it is seen to exit, so
  a session already listed with no pid (stopped moments ago by `--stop`, a turn stopped
  during its launch, a wait that ran out, or a listing that failed) is waited for the same
  way before the resume; a pid whose start time changed belongs to another process and is
  not waited for. Then `claude --bg --resume <session id> --
  "<text>"` with no other flag: it "wakes" the session in place with its saved options
  (`--settings` hooks, `--remote-control`, `--model`...), while any flag passed with
  `--resume` makes Claude Code start a copy, which comes back without the hooks (measured).
  A resume of a session that is still running also starts a copy, and "running" lasts until
  Claude Code settles the killed process: measured on 2.1.301, `claude stop` returns and the
  listing drops the `pid` up to two seconds before the process exits and the daemon logs
  `settled (killed)`; a resume in that window started a copy that kept none of whisrs's
  flags (no `--remote-control`, no hooks, permission mode `auto`). A cap that is hit fails
  the turn ("Claude Code did not stop the session's last run in time, so this turn was not
  sent."). **A copy is never adopted:** a resume that prints another bg id, or says it
  "started a copy as <id>", has that copy stopped, waited for like the original and removed
  (`claude stop`, the wait, `claude rm`, which fails while its process runs, warned with its
  id; its id came from our own launch output, so it is ours) on the launch's own task, so a
  `--stop` cannot leave it running, and never reported. The copy is the id the note names,
  else a printed bg id other than the session's: never the session's own bg id (a note that
  names it, with that id printed, is taken as continued in place). The original is stopped
  and waited for again and the resume tried once more. A second copy fails the turn: "Claude Code could not
  continue the session, so this turn was not sent. `whisrs agent <name> --new` starts a fresh
  one."
- **Turn end:** the `Stop` event whose `session_id` matches carries `last_assistant_message`
  and `transcript_path`. Hook events carry when the daemon received them; for a resumed
  turn only a `Stop` received after the resume launch was spawned counts (the old process is
  stopped first, so anything earlier is its own: a `Stop` with a message from it is spoken
  as a reply the session produced on its own, `TurnUpdate::SessionReply`). A turn watched
  again after a restart takes every event. A restart that landed during a first launch,
  before its bg id was printed, leaves a launch that survives in its own scope and a turn
  with no id to watch: it ends with "whisrs lost track of that turn after a restart, and it
  may still be running." (never "say it again", which could run the instruction twice), and
  the session is not looked for by name. A `Notification` for a permission prompt marks the agent needs-you and
  speaks a short notice (for example "Waiting for your approval to use Bash"); ignore idle
  notifications. A voice turn while needs-you is sent normally (stop + resume).
- **Fallback poll** while a turn runs, every ten seconds: `claude agents --json --all`, our
  bg id only. The process decides first: an exited entry keeps its last state, so no `pid`
  → the turn ended ("the session ended before replying", or the reply from the transcript
  when its last state was `done`); not listed → "the session ended" and the session is
  forgotten. `done` waits two seconds for the Stop hook (hook events are still handled
  meanwhile), then reads the last assistant text since the turn started from the transcript
  JSONL; `done` with no text for three polls in a row ends the turn with "That turn was
  interrupted, say it again." (a restart that landed between the stop and the resume).
  `blocked` → needs-you. A watch that knows only the bg id learns the session id here.
  Measured: Claude Code restarts a session whose process was killed about ten seconds later
  ("automatically restarted after its process exited unexpectedly") and the turn goes on;
  whisrs has ended the turn with an error by then, and the session's own `Stop` is spoken
  as a reply it produced on its own.
- **Stop:** `claude stop <bg id>`.
- **Untrusted dir:** `--bg` refuses a directory never trusted in Claude Code. Detect it and
  notify "Open <dir> in Claude Code once and accept the trust prompt."
- **Never touch sessions whisrs did not start.** Match by stored ids only, never by name.

## Spoken reply

Default instruction (appended system prompt on Claude, prefixed to the first message on a
harness without one):

> You are being driven by voice through whisrs. The user spoke this request, and speech
> recognition can mishear words, so read it charitably; if a misheard word could make you do
> something destructive or irreversible, ask first. Work as you normally would. The user will
> hear your final message through text-to-speech rather than read it, so end every final
> message with a spoken reply inside `<spoken></spoken>` tags, written to be heard:
> lead with the outcome; one to three short sentences, under 40 words, up to 60 only when
> you need a decision; plain spoken words with no code, file paths, URLs, markdown, lists or
> symbols; say numbers the way a person would. If you need the user to decide or answer
> something, end with exactly one direct question; otherwise ask nothing and do not offer
> more help. Do not narrate your steps, apologize or repeat the request. Be calm, precise
> and brief, like a capable assistant reporting to the person they work for. Anything
> outside the tags stays on screen for when the user looks at the session.

whisrs never trusts the model to comply (users run different models, some small):

- Take the last complete `<spoken>…</spoken>` pair: each closing tag pairs with the last
  opening tag before it, so a tag the reply mentions after its block is not one. Only
  without a complete pair does a block cut off at the end count. Without any, take the whole
  reply and keep its first two sentences.
- Sanitize: drop fenced code; inline code of more than three words dropped, shorter kept as
  words; `[text](url)` → text; bare URLs and file paths dropped; markdown markers, list
  bullets and table rows stripped; whitespace collapsed.
- Cap at 400 characters on a sentence boundary, so every TTS backend takes it in one request.
- Nothing speakable after all that (no letter or digit, `<spoken>https://x.io!</spoken>`)
  → "Done."
- Pure functions in `src/agent/spoken.rs`, unit tested.

## State and persistence

- `src/daemon/agent.rs` owns an agent runtime in `DaemonContext` behind its own lock, outside
  `DaemonState`. The state machine only gains the agent recording sink.
- `$XDG_DATA_HOME/whisrs/agents.json` (0600): per agent `harness`, `dir`, `session_id`,
  `bg_id`, `transcript_path`, `turn_running`, `turn_started_at`, `updated_at`. Written on
  every change, off the
  async runtime (`spawn_blocking`, numbered snapshots so the newest always lands last), as is
  every transcript read. At startup an agent with `turn_running` is watched again (poll), so
  a reply that lands after a daemon restart is still spoken. A changed `harness` or `dir`
  starts a fresh session.
- Daemon shutdown leaves agent sessions running; they belong to the harness.

## Layout

```
src/agent/mod.rs        # driver abstraction, TurnOutcome, permissions mapping, instruction
src/agent/spoken.rs     # <spoken> extraction + sanitize (pure)
src/agent/claude.rs     # Claude Code driver
src/agent/store.rs      # agents.json
src/daemon/agent.rs     # runtime: slots, turns, reply FIFO, IPC handlers, hook events
```

The driver abstraction must fit both shapes: Claude's turn ends on an event or a poll;
phase 2 drivers run one child process per turn and read the session id and final message
from its JSON output (`kill_on_drop`). Adding one must not change the runtime.

## Phase 2 drivers (same PR, after the phase 1 test)

- OpenCode: `opencode run --format json [--session <id>]`, or `opencode serve` (HTTP + SSE).
- Codex: `codex exec --json`, `codex exec resume <id>`.
- GitHub Copilot CLI: from its docs.
- Each follows its own documentation; contracts are collected before phase 2 starts.

## Later

- Approving a permission prompt by voice mid-turn, via ACP (Agent Client Protocol).

## Acceptance criteria (phase 1)

1. No `[[agents]]`: no behavior change, no processes spawned, no files written.
2. `whisrs speak --text` and `--text -` speak the given text; an older daemon errors.
3. `whisrs agent jarvis --text …` runs a real `claude --bg` turn in `dir` and speaks the
   sanitized `<spoken>` part. A second turn continues the same session (it remembers turn 1).
4. A voice turn works through `whisrs agent jarvis` and through the configured hotkey, on a
   streaming and a batch backend. The hotkey listener starts when only agents have hotkeys.
5. A reply that lands while recording is spoken once the daemon is Idle. The agent hotkey
   while Speaking stops playback and starts recording.
6. A press while that agent's turn runs is refused with a short notice; no second turn.
7. `--stop`, `--new`, `--reset`, `new_session_hotkey` and `whisrs agent` work. A turn in
   flight across a daemon restart is still spoken after it. Resumed turns continue in place
   (same session id, no copy, ended by the `Stop` hook, `--remote-control` kept).
8. Missing binary, untrusted dir and a vanished session each give an actionable notification
   and leave the agent usable.
9. Sessions whisrs did not start are never stopped, resumed or read.
10. Docs: `docs/configuration.md`, README, `contrib/whisrs.1`, CLAUDE.md (structure, IPC,
    running).
11. The full CI gate in CLAUDE.md passes.
