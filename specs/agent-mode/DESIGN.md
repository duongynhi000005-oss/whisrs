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
dir = "~/Projects/whisrs"        # required, ~ expanded, relative = from whisrsd's cwd, must exist
permissions = "auto"             # auto (default) | bypass | ask
command = "claude"               # optional binary path; default: the harness name on PATH
args = ["--remote-control", "jarvis"]   # optional, passed on the first turn of a session
reply_instruction = "..."        # optional, replaces the built-in voice instruction
feedback = "brief"               # summary | brief (default) | conversational

[agent_overlay]                  # optional; drawn only with an [[agents]] entry
enabled = true                   # default: follows [general] overlay
style = "orb"                    # orb (default) | face
corner = "bottom-right"          # bottom-right (default) | bottom-left | top-right | top-left
```

- `permissions` is harness-neutral and mapped per driver. No warning for `bypass`: the user
  chose it. Claude: `auto` → `--permission-mode auto`, `bypass` → `--permission-mode
  bypassPermissions`, `ask` → `--permission-mode default`. `ask` passes the flag rather than
  none: without it a session starts in the user's `permissions.defaultMode`, which may be
  `auto`, and `ask` would not ask. OpenCode: `auto` → no flag, `bypass` → `--auto`, `ask` →
  shell and edit ask rules appended to `OPENCODE_CONFIG_CONTENT` (see "OpenCode driver").
  Codex: `auto` → `--approve-for-me`, `bypass` → `--dangerously-bypass-approvals-and-sandbox`,
  `ask` → `--sandbox read-only` (see "Codex driver"). Copilot: `auto` → `--allow-all-tools`,
  `bypass` → `--yolo`, `ask` → `--deny-tool=shell --deny-tool=write` (see "Copilot driver").
- `[[agents]]` (and `[agent_overlay]`) is read once, when the daemon starts
  (`AgentRuntime::new`): any change needs `whisrs restart` first.
- `permissions`, `args` and `reply_instruction` are fixed when a session starts (a resume
  passes no flag, see below), so after the restart a change applies from the next `--new`.
  OpenCode passes `permissions` on every turn (`run` keeps none), so a change applies from the
  next turn after the restart; `args` and the instruction are fixed at session start there too.
  Codex passes `permissions` and `args` on every turn (`exec resume` keeps neither, measured);
  the instruction is fixed at session start there too. Copilot passes `permissions`, `args` and
  the instruction on every turn (a resumed turn keeps none of them, measured), so all three
  apply from the next turn after the restart.
- `dir` is made absolute at load (`agent::absolute_dir`): `~` expanded, a relative path joined
  to the daemon's working directory, `.` dropped (`..` and symlinks kept). Every harness gets
  it; OpenCode takes its directory from `PWD`, which a relative dir would leave meaningless
  once the child is in it (every turn failed).
- Validation warns (and skips the entry, `resolve_agents`) on a missing, invalid (not
  `[a-z0-9_-]`) or duplicate name, an unknown harness or `permissions` value, and a missing
  or nonexistent dir; a hotkey clash is reported like the `[[llm_commands]]` clashes. Agents
  with `[tts] enabled = false` get one warning: replies arrive as notifications only.
- `new_session_hotkey` has the syntax and clash checks of `hotkey` (it is one more binding
  source, so it also clashes with the agent's own `hotkey`), and starts the hotkey listener on
  its own.
- `command` exists because a systemd user unit often lacks `~/.local/bin` on PATH. A spawn
  failure names it.
- `feedback` needs the restart like every key. Its request is appended to the instruction,
  so it is fixed at session start like `reply_instruction`: a higher level applies from
  the next `--new` after the restart. A lower one applies from the restart, since the
  daemon's gate (`ProgressGate`, built per turn from the configured level) filters what is
  spoken. An unknown value warns and means `brief`; the entry is kept.
- `[agent_overlay]`: an unknown `style` or `corner` (an edge center too) gets one warning
  and the default. The keys are known to the unknown-key check.

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
   instruction recording (cues, both streaming and batch paths), with the agent as the
   sink instead of the LLM rewrite. It shows on the agent overlay, not the pill or the
   tray (see "Agent overlay"). No selection capture. Nothing to send (audio too
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
  cgroup), `--bg` launches run under `systemd-run --user --scope --quiet --collect
  --expand-environment=no --` (environment, cwd and pipes kept); a missing `systemd-run`, or
  one that cannot reach the user manager, falls back to a direct spawn. Since systemd 254 a
  scope's command line has `$NAME` and `${NAME}` expanded like a service's `ExecStart=`
  (measured on 262: `$HOME` arrived as the home directory), which would change the instruction
  or the user's words; `--expand-environment=no` keeps them. An older systemd-run refuses the
  option (and expands nothing), so the launch is retried without it
  (`claude::expand_option_refused`). `claude agents` and `claude stop` never start
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
  else a printed bg id other than the session's: never the session's own bg id. A note that
  names the session itself, with that id printed, is unclear (it may have continued in
  place, or run as a copy): that run is listed until it shows a pid, so its process is
  remembered (`claude stop` drops the pid from the listing before the process exits), then
  stopped and waited for, never removed. The original is stopped and waited for again and
  the resume tried once more. A second copy fails the turn (a second unclear run is stopped
  first, so a turn reported as not sent is not running): "Claude Code could not
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
  notifications. A voice turn while needs-you is sent normally (stop + resume). With no
  turn running, any `Stop` for the session (with no message too, `IdleEvent::Ended`) ends
  a needs-you or own-work row (see "Agent overlay").
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

## Feedback levels and the transcript tail

What an agent says before its reply is written by the agent itself and filtered by
whisrs (`src/agent/progress.rs`):

- `summary`: the reply only (the instruction is unchanged).
- `brief`: the instruction asks for a separate two-to-six-word acknowledgement in its own
  `<spoken>` tags before the first tool call ("On it.", "Let me check."; skipped when the
  agent answers without a tool), and no other tags until the final message.
- `conversational`: the acknowledgement, and at a real milestone one short update (one
  sentence under twelve words) in a message before the next tool call.
- `ProgressGate` (one per turn) admits what the configured level allows, whatever the
  model writes: summary nothing; brief the first line, as the acknowledgement;
  conversational that, then updates at least `PROGRESS_MIN_GAP` (20 s) after the last
  admitted one and never one said already (`same_reply`). Acknowledgements depend on the
  model following the request.
- Only a message a tool call followed can be an interim, so the final reply never is:
  `InterimTracker` holds a text block with a complete `<spoken>` pair until a later
  `tool_use` in the same turn proves it was not the last (text, text, end speaks nothing
  extra). `spoken::interim_line` takes the pair, sanitizes it and caps it at 120
  characters; nothing speakable says nothing.
- Interims queue in the reply FIFO under the same holds, as `Utterance::interim` with an
  expiry (`ACK_TTL` 8 s, `PROGRESS_TTL` 12 s; an expired one is skipped), are dropped when
  that agent's reply or error arrives, on `whisrs cancel`, `--stop` and `--reset`, are
  never requeued after a recording stopped them, and are never notified.
- Source: `TranscriptTail` (`src/agent/claude_tail.rs`) follows the session's transcript
  JSONL while a turn runs, from the first statement of `ClaudeDriver::watch` (first turn,
  resume and rewatch): every 750 ms, the whole file filtered to the turn's start first,
  then from where it stopped, holding a partial last line; sidechain text and thinking are
  skipped. Each `tool_use` sends `TurnUpdate::Activity` (the overlay's activity line, from
  `activity_for_tool`), and a held text block a tool call followed sends
  `TurnUpdate::Interim`. A rewatch after a restart sends activities only. Not hooks: hooks
  have no assistant-text event, `PreToolUse` would spawn a process per tool call, and
  hooks are fixed at session start (a resume passes no flag). Phase 2 harnesses map their
  event streams onto the same two updates.
- Activity labels: Bash "Running a command", Read "Reading files", Edit/Write "Editing
  files", Grep/Glob "Searching the code", WebFetch "Reading a web page", WebSearch
  "Searching the web", Task/Agent "Working with a helper", TodoWrite "Planning"; MCP tools
  by server keyword (browser, mail, calendar, whisrs), else "Using <Server Words>";
  anything else "Working". Case-insensitive, phase 2 tool names included.

## Agent overlay

Agent mode no longer drives the pill or the tray; it has an overlay of its own, a row per
active agent in a screen corner.

- **StateBus** (`src/daemon/context.rs`): `DaemonContext::state_tx` holds three watch
  channels, all sent from one `DaemonState` under its lock by `publish(&ds)`, the only way
  to send. `raw` is the state machine as before (recording-lifecycle hooks, so media pause
  still fires for an agent's voice turn, and the reply queue). `ui` is the same but Idle
  while the voice belongs to an agent (`DaemonState::agent_voice`): the tray, the pill and
  the GNOME pill signal. `voice` names that agent and its state, for the board. The voice
  is an agent's while its voice turn records (`agent_recording`), is transcribed
  (`agent_transcribing`, set at the claim and cleared at the transcript or a takeover),
  and while its utterance is synthesized or played (`tts_agent`, recorded by `speak_text`
  from `AgentRuntime::playing_agent`, only with `tts_origin == Agent`). A plain toggle
  that takes over an agent recording shows the pill at Transcribing, as dictation.
- **Board** (`src/agent/display.rs`, pure): per agent its running turn (activity line),
  a wait for you, how its last turn ended and whether an utterance of it is queued or
  playing, plus the one agent the voice belongs to. `compose(now)` gives rows in config
  order, each in the state that wins: voice, then needs-you, then working, then done or
  error. A done or error row holds while its agent's utterance is pending, then for
  `DONE_HOLD` (2.5 s), then fades over `DONE_FADE` (0.6 s). `clear` (`--stop`, `--reset`,
  the end of a session's own work) keeps the voice, so a reply or recording of that agent
  still shows until the voice moves on.
- **Timer:** during a turn the card counts the turn's time while working, needs-you and
  speaking (an acknowledgement or notice mid-turn); listening and transcribing count from
  the press, and so does speaking with no turn running. A finished row reads "Finished in
  m:ss" or "Failed after m:ss" and shows no timer.
- **Board task:** the runtime sends `BoardEvent`s (turn started, activity, needs-you,
  working, turn ended, pending agents, clear) on an unbounded channel created only with
  agents; `board_loop` owns the `Board`, also reads `voice`, and publishes rows on
  `DaemonContext::agent_board` when they change, waking on its own when a fading row ends.
- **The session's own work:** a turn refused because the session works on its own
  (`Busy::Background` at the start, or `TurnOutcome::Busy` from the driver) shows as
  working, matching `whisrs agent`, not as a failed turn; a permission prompt with no turn
  running shows needs-you. Either row ends on any `Stop` for the session (with a message:
  a Done row while the reply plays; without one, `IdleEvent::Ended`: gone), or when a
  listing shows the session neither working nor waiting: while such a row shows,
  `idle_loop` asks `Driver::session_phases` (one `claude agents` for every such agent)
  10 s after the rows shown changed, then doubles the wait up to 120 s (`IDLE_POLL`,
  `IDLE_POLL_MAX`), so a row shown all day (a dev server) or a listing that keeps failing
  is not listed every 10 s forever. The rows shown changing, or a turn starting, starts
  the wait over (never later than it was due). `whisrs agent` listings count too. A
  listing only follows a row, never starts one. A wait shown with no turn running that
  a `Stop` with a reply ends is Done while the reply plays, like the same work shown
  working first (`Board::turn_ended` times it from the wait).
- **Needs-you notice:** queued under `WAITING_KEY`, one per agent; a newer prompt replaces
  the queued notice's text, and one already playing plays on with the newer text queued
  once after it. When the one playing is stopped and put back (`requeue_front`: a
  recording start, or `NotIdle`), a newer one of its key queued meanwhile takes the front
  instead, so two are never queued.
- **Hosts** (`src/overlay/`, each on its own thread and display connection, apart from the
  pill): Wayland: a layer-shell surface (namespace `whisrs-agents`, `Layer::Overlay`,
  anchored to the corner's two edges, no input region, no keyboard), a fixed logical
  300x236 canvas. Its loop sleeps in `poll` on the Wayland fd and a wake socket the board's
  forwarder writes a byte to per update; while a row shows it draws on frame callbacks at
  most every 33 ms, and with none it requests no callback and commits nothing, the last
  transparent buffer staying attached. A surface coming back from hidden with several
  outputs is recreated on the focused one; a recreated surface starts at scale 1 again.
  It follows the integer buffer scale; a fractional scale draws at the next whole one,
  scaled down by the compositor (fractional scaling is deferred). A compositor-closed
  surface clears `AgentOverlayShown` until a new one is configured. X11: an
  override-redirect window like the pill's, shaped to what it draws and click-through,
  unmapped and blocked on its wake channel while idle, scaled by `Xft.dpi / 96` (1 to 3).
  GNOME on Wayland: no native surface (no layer-shell); `gnome.rs` broadcasts
  `AgentsChanged` (JSON rows, style, corner) on the existing bus name on every change and
  each second while a row shows, and the extension does not draw it yet. GNOME on X11 uses
  the X11 host.
- **Toasts:** the "Speak to 'X'" and "Sending to 'X'" notifications show (per `[general]
  notify`) only while the agent overlay is not on screen (`notify_agent_state`), which is
  the GNOME Wayland fallback.
- **Rendering** (`agent_render.rs`, headless, unit tested): a 44 px shape (`agent_orb.rs`
  or `agent_face.rs`, seven states each) on the screen-edge side of a 212x44 card: name
  and state word, activity and `m:ss`. Rows stack away from the corner's edge. Text is
  `fontdue` with ASCII subsets of Inter Regular and SemiBold in `src/overlay/font/` (OFL),
  rasterized at the physical size.

Deviations from the plan, as built:

- `AgentScene` is stateful: it remembers per row when it appeared and when its state or
  activity changed, so the 180 ms entrance plays once and a change crossfades. An entry's
  `since` cannot carry that (it restarts with a voice state, and a Done row's is the
  turn's start).
- "+N more" is a count, not an agent: with more than four rows, three show and a chip in
  the fourth slot says how many more.
- The face's working dots sit in its upper right, inside its box.
- X11 reshapes grow then shrink: an image only lands inside the bounding shape, and
  Xwayland composites the whole window, so the shape first grows to the old and the new
  one, the frame clears what leaves it, and only then does it shrink (no trails).
- `Driver::forget` (required, no default) was added: the runtime calls it when it forgets
  a session (`--new`, `--reset`), so a driver drops what it remembers about it (Claude's
  remembered pids).

## State and persistence

- `src/daemon/agent.rs` owns an agent runtime in `DaemonContext` behind its own lock, outside
  `DaemonState`. The state machine only gains the agent recording sink; `DaemonState`
  also says whose voice it is (`agent_transcribing`, `tts_agent`) for the StateBus.
- `$XDG_DATA_HOME/whisrs/agents.json` (0600): per agent `harness`, `dir`, `session_id`,
  `bg_id`, `transcript_path`, `session_pending` (`SessionRef::pending`), `turn_running`,
  `turn_started_at`, `updated_at`. Written on
  every change, off the
  async runtime (`spawn_blocking`, numbered snapshots so the newest always lands last), as is
  every transcript read. At startup an agent with `turn_running` is watched again (poll), so
  a reply that lands after a daemon restart is still spoken. A changed `harness` or `dir`
  starts a fresh session.
- Daemon shutdown leaves agent sessions running; they belong to the harness. A
  process-per-turn turn (OpenCode, Codex, Copilot) ends with the daemon (see "The process-per-turn
  base"); its session stays, and the next turn continues it.

## Layout

```
src/agent/mod.rs        # driver abstraction, TurnOutcome, permissions mapping, instruction
src/agent/spoken.rs     # <spoken> extraction + sanitize, interim lines (pure)
src/agent/claude.rs     # Claude Code driver
src/agent/claude_tail.rs  # transcript tail: activities and interims
src/agent/process.rs    # process-per-turn base: ProcessDriver<H: TurnHarness>, StreamState, stop
src/agent/opencode.rs   # OpenCode driver (a TurnHarness)
src/agent/codex.rs      # Codex driver (a TurnHarness)
src/agent/copilot.rs    # GitHub Copilot CLI driver (a TurnHarness)
src/agent/progress.rs   # feedback levels, instruction deltas, labels, tracker, gate (pure)
src/agent/display.rs    # the agent overlay's Board and rows (pure)
src/agent/store.rs      # agents.json
src/daemon/agent.rs     # runtime: slots, turns, reply FIFO, IPC handlers, hook events,
                        #   board_loop, idle_loop
src/daemon/context.rs   # StateBus (raw, ui, voice)
src/overlay/layer.rs    # layer-shell bits the pill and the agent overlay share
src/overlay/agent_host.rs, agent_wayland.rs, agent_x11.rs   # agent overlay hosts
src/overlay/agent_render.rs, agent_orb.rs, agent_face.rs, text.rs, font/   # its drawing
```

The driver abstraction must fit both shapes: Claude's turn ends on an event or a poll;
phase 2 drivers run one child process per turn and read the session id and final message
from its JSON output (`kill_on_drop`). Adding one must not change the runtime.

## Phase 2 drivers (same PR, after the phase 1 test)

- OpenCode: built, see "OpenCode driver".
- Codex: built, see "Codex driver".
- GitHub Copilot CLI: built, see "Copilot driver".
- Each follows its own documentation; contracts are collected before phase 2 starts.

### The process-per-turn base (`src/agent/process.rs`)

A harness whose turn is one process implements `TurnHarness`, every method required:
`label` (for messages), `launch` (agent, session, request → `Launch`: the args after `command`,
the prompt for stdin or `None` for `/dev/null`, extra env), `strips_env` (its parent-session
variables), `events` (one stdout line → `StreamEvent`s: `Session(id)`, `Text { message, text }`,
`Tool(name)`, `Error(message)`, `RecoverableError(message)`) and `stderr_failure` (a failure it recognizes in the last stderr
lines, such as a session it no longer has, else `None`). `ProcessDriver<H>` is the `Driver`:

- **Spawn:** `command` in `dir`, own process group, `PWD` = dir, the daemon's environment minus
  `claude::strips_env` (Claude's parent-session list and `WHISRS_*`) and the harness's own list,
  then `Launch::env`. In a transient scope of its own: `systemd-run --user --scope --quiet
  --collect --expand-environment=no --unit=whisrs-agent-<instance>-<agent>-<turn>.scope --
  <command> <args>` (`claude::scope_args`; `<instance>` is eight hex digits of a hash of the
  daemon's `agents.json` path, so a test daemon beside the user's never touches the user's
  scopes, `<turn>` eight random ones). systemd-run execs the harness as the same process (pid,
  group, pipes, environment and PDEATHSIG kept; systemd adds `INVOCATION_ID`), and every process
  the turn starts is in the scope's cgroup whatever its group or session. A start that never ran
  the harness (no stdout line, a failed exit, stderr of systemd-run's own) is retried: without
  `--expand-environment` when an older systemd-run refused it, directly when it could not make
  the scope or found no harness (`Wrap::after`; a missing `systemd-run` goes direct at once).
  Spawned from a thread that registers the child for `--stop` before the turn hears of it,
  writes the prompt, closes stdin and waits for the child, with `PR_SET_PDEATHSIG` = SIGINT
  (checked against `getppid` after the prctl): the thread lives as long as the child, so the
  child is interrupted only when the daemon exits or dies.
- **Stop before registration:** right after registering, the thread reports the launch's new
  session id (`TurnUpdates::report_session`) or reads the stop flag (`TurnUpdates::stopped`), in
  one step with the runtime's `TurnUpdates::stop` (which comes before `Driver::stop`): either
  that stop finds the process registered, or the thread sees the stop, leaves the prompt
  unwritten and stops the process itself with the whole escalation (`stop_detached`), as it does
  for a process whose turn was dropped while it spawned. It stays registered until it is gone,
  so the next turn waits for it.
- **Session:** a session the launch names (`Launch::session`, OpenCode's or Copilot's minted id) that the
  agent does not have yet is reported at spawn with `pending = true` (`SessionRef::pending`,
  persisted), so a turn stopped or cut off before its first line keeps it; the first `Text` or
  `Tool` of a turn confirms it (reported with `pending = false`). An id the output names that
  differs from the agent's is reported confirmed. A start that never ran the harness (not found)
  reports the agent's old session back.
- **Output:** stdout lines go through `events` into `StreamState`: `Tool` sends `Activity`
  (deduplicated) and releases the text before it through `InterimTracker`; the reply is the text
  parts of the last message, joined (an empty last message is no reply, and so is a skipped
  over-long stdout line after the last text: it may have been the final message, and the text
  before it, often the acknowledgement spoken already, is never the reply). Stderr keeps its last 40
  lines. A line is read through a capped reader (`LineReader`, cancel safe like `read_until`):
  a stdout line over 1 MiB (`STDOUT_LINE_MAX`) is skipped, a stderr line over 8 KiB
  (`STDERR_LINE_MAX`) keeps its start, each with one warning per turn; neither is ever buffered
  whole. Reading ends when both pipes end and the process exited, or two seconds after the exit
  (a leftover process can hold stdout open).
- **Outcome:** an `Error` event fails the turn ("Agent 'x': OpenCode failed: ..."), and so does
  a `RecoverableError` no `Text` came after; else the reply (a non-zero exit after it is only logged); else "ended the turn without a reply" on exit
  0, or what `stderr_failure` recognizes, or the stderr line that mentions an error (else its
  last), or the exit status.
- **Stop:** SIGINT, SIGTERM, SIGKILL (`STOP_GRACES` 3 s, 2 s, 2 s), each only while something
  of the turn is left, to the group (never `kill(0)` or `kill(-1)`), to every process of its
  tree seen so far (`Tree`: `/proc/<pid>/task/*/children` walked from the leader and from every
  process seen, each kept with its start time, so a `setsid` child is found through its parent
  and still signalled once that parent exited; walked at each step and each 50 ms poll), and
  to the scope (`systemctl --user kill --signal=<SIG> <unit>`). Gone = the exit seen,
  `kill(-pgid, 0)` = ESRCH, no tree process alive and the scope's `cgroup.procs` empty (its
  cgroup from `systemctl --user show -p ControlGroup`, cgroup v2). Then `systemctl --user stop
  --no-block <unit>` as a backstop. Run once per process (`OnceCell`), so `Driver::stop` after
  the abort and the stop a dropped turn starts (`Registered`'s drop) are the same stop, and
  both wait for it. The next turn of the agent waits for a previous process still stopping:
  never two on a session. Copilot's tool shells take a session of their own and survive a
  SIGKILL of the group (contract 3.6), which is why the tree and the scope exist. Without a
  scope, a process whose parent exited before any walk saw it is out of reach.
- **Restart:** the scope is outside the daemon's cgroup (`whisrs.service`'s, under systemd), so
  a stopped or restarted daemon no longer takes the turn along: PDEATHSIG interrupts the
  harness, and `TurnScopes::sweep` stops anything an earlier turn left in its scope, a
  background job included (a process that ignored that SIGINT, or one a turn that ended
  normally left running, as `nohup npm run dev &`), when the next daemon starts, as the old
  service-cgroup kill on restart did. A daemon with no agents sweeps too
  (`agent::sweep_without_agents`, one `systemctl` listing), so a turn left by a daemon that died
  before `[[agents]]` was emptied does not run forever: `systemctl --user list-units --all --plain
  --type=scope 'whisrs-agent-<instance>-*'`, then the same escalation on each scope still
  running (`Scope::stop_leftover`). Spawned by `start_background`, run once (`OnceCell`), and
  awaited by every turn and rewatch before they spawn anything. Measured with a fake harness
  that starts `setsid sleep 300 &` (SIGINT ignored, a background job) and sleeps: `--stop`
  returned after 3.08 s (the SIGTERM after the first grace) with no `sleep 300` left, also when
  its parent had exited at once (an orphan, reached only through the scope); a SIGKILL of the
  daemon left the `sleep 300` running until the restarted daemon's sweep stopped it, with no
  scope left. Without a reachable user systemd the turn started directly, `--stop` still ended
  the setsid child (the tree) but not the orphan. Real runs (mock models): `--stop` mid tool
  call left no `opencode`/`codex`/sandbox/`sleep` process (191 ms OpenCode, 90 ms Codex), and so
  did a SIGTERM or SIGKILL of the daemon mid-turn; the next turn continued the session.
- **Rewatch:** the turn ended with the old daemon, so it fails at once with `cut_off`: "whisrs
  restarted during that turn, which stopped it before OpenCode replied. The session keeps what
  it did so far." For a session the agent was never seen working in (none yet, or pending):
  "... It may not have reached OpenCode at all, so say it again." It never stays working.
- Between turns nothing runs: `session_phase(s)` is Idle, no hooks (`hook_matches` false,
  `idle_event` ignored), `forget` has nothing to drop. No needs-you: none of the three harnesses
  waits on an approval headless.
- A stop that lands while the process is being started (before it is registered) finds nothing
  for `--stop` to wait for, which returns at once; the spawning thread sees the stop and stops
  the process in the background, and the agent's next turn waits for that.

### OpenCode driver (verified on 2.0.29)

- **Every turn:** `opencode run --standalone --format json --session <id> [--title
  whisrs-<agent> args…] [--auto]`, the prompt on stdin (argv words with spaces reach the model
  quoted). `--standalone` gives the run a private server (`opencode serve --stdio`, in a session
  of its own, ended by its stdin lease when the run exits): the shared background service would
  outlive the turn, ignore the run's environment and keep a turn going when `run` is killed.
- **Session id:** whisrs mints it in OpenCode's layout (`ses_` + twelve hex digits of the
  inverted `ms * 4096 + counter`, so newer sorts first, + fourteen base62 characters,
  `opencode::new_session_id`); `--session` creates a session it does not find, so the same
  command continues it later. It is reported at spawn, pending (see the base), so a first turn
  stopped or cut off before its first line keeps it and the next turn continues it instead of
  leaving a `whisrs-<agent>` session behind. `--title`, `args` and the instruction go on the
  first turn, and again on every turn while the session is pending (the title also saves the
  title-generation model call, measured); a `-m` model or `--agent` stays the session's
  (measured). OpenCode keeps the title of a session it has (a second `--title` changed nothing)
  and takes the rest again (measured on 2.0.29: a first turn that failed on `-m nosuch/model`,
  then the same id with `-m mock/mock-1`, ran on the new model in the same session, the
  instruction twice in its history), so a first turn that only errored does not pin a typo in
  `args`, and one stopped before OpenCode made the session leaves none without the instruction.
- **Instruction:** prefixed to the first prompt, then a blank line. `run` has no system prompt
  option; `agents.<name>.system` replaces OpenCode's base prompt and the session then cannot
  resume without that agent; config `instructions` did not reach the model; `AGENTS.md` would
  mean writing into the user's project. In the history it holds for every turn (measured); the
  tradeoff: it shows in the session's first message, and a compaction can summarize it.
- **Permissions:** headless OpenCode answers every ask with a rejection plus feedback to the
  model, which goes on (no hang, exit 0). `auto` passes nothing (OpenCode's defaults allow most
  tools); `bypass` passes `--auto` (asks approved, deny rules hold); `ask` appends
  `[{shell, *, ask}, {edit, *, ask}]` (edit, write and patch all assert `edit`) to
  `OPENCODE_CONFIG_CONTENT`, after the daemon's own content (merged; rules are last-match-wins,
  and the env layer comes after the config files, measured). OpenCode's own saved approvals
  ("always" answers) and a session's allows come after every config rule, so they win: `ask`
  refuses commands and edits unless OpenCode already has an approval saved for them. Content
  that is not a JSON object fails the turn rather than running without the rules.
- **Events:** `text` → `Text { part.messageID, part.text }` (printed when the part ends; a fast
  turn's texts come after the turn, from the run's reconcile, in order); `tool_use` →
  `Tool(part.tool)`, printed when the call ends (completed or rejected), so the activity and the
  acknowledgement before it come then; `error` → `Error(error.message)`; `step_start`,
  `step_finish` and `reasoning` only name the session. The turn ends at the process's exit.
- **Stop:** SIGINT to the run interrupts the session and exits 130 (0.2 s, measured); the
  server and the tool's shell (its own session) go with it, and the next turn continues the
  session with the stopped call in its history (measured, also after SIGTERM to every process
  at once, as a cgroup kill does).
- **Restart:** `PR_SET_PDEATHSIG` interrupts the run when whisrsd exits or is killed (measured),
  the next daemon sweeps its scope, and the restarted daemon's rewatch ends the turn with
  `cut_off`.
- **Environment:** besides the base's stripping, `OPENCODE`, `OPENCODE_SESSION_ID`,
  `OPENCODE_TERMINAL`, `AGENT` (set in OpenCode's tool shells), `OPENCODE_PASSWORD`,
  `OPENCODE_SERVER_PASSWORD`. `OPENCODE_CONFIG_CONTENT`, `OPENCODE_CONFIG*` and the XDG
  directories pass.
- **List:** `whisrs agent` shows the whole session id when there is no bg id (what
  `opencode --session` takes); Claude's rows keep the short id beside the bg id.
- OpenCode 1 (no `--standalone`, `--session` only continues) is not driven.

### Codex driver (verified on 0.164.0 against a local mock model)

Tested with the npm-packed `codex` 0.164.0, an isolated `CODEX_HOME` whose `model_providers.mock`
points at a scripted Responses API server; no account and no real model.

- **Every turn:** `codex exec --json <permissions> -c analytics.enabled=false -c
  developer_instructions=<TOML string> [args…] --skip-git-repo-check -`, the prompt on stdin; a
  later turn adds `resume <thread id>` before the `-`. Every option goes before `resume`
  (`--sandbox` and `--approve-for-me` are a usage error after it). The skip lets `dir` be any
  folder (without it Codex exits 1 outside a git repo), and comes after `args` because exec's
  `-i`/`--image` takes `1..` values: `-i x.png resume <id> -` takes the resume as image files
  (0.164.0's parser: `--help` after it printed `codex exec`'s help, not `codex exec resume`'s),
  while a flag after `-i x.png` ends it (`resume`'s help, and `-` parsed as the prompt). The
  flag may not be given twice (clap refuses it), so `args` must not hold it.
- **Session id:** Codex mints it; the first line is `thread.started{thread_id}` (a UUIDv7), and a
  resumed turn repeats the same id. Only a UUID is taken (`codex::is_thread_id`) and resumed: a
  stored id that is not one fails the turn, since Codex would look it up as a thread name (and
  start a new thread when none matches). A turn that failed after `thread.started` (a provider
  400) keeps a resumable thread (measured), and is never pending: nothing is first-turn only
  (every turn passes the permissions, `args` and the instruction), so keeping it pins nothing. A thread Codex no longer has fails the resume on
  stderr (`no rollout found for thread id`, exit 1), which `stderr_failure` turns into "Codex no
  longer has this agent's session ... `--new` starts a new one".
- **On every turn:** `args` (a `-m` model of turn 1 was gone on turn 2, measured) and the
  permission flags (a resumed turn applies the flags it is given, measured from read-only to
  bypass, to auto, back to read-only).
- **Analytics:** with analytics on (Codex's default), `codex exec` uploads metrics after
  `turn.completed` and exits only after the upload, 4 to 10 s later (measured, every run), and the
  reply waits for the exit. `-c analytics.enabled=false` makes it exit about 25 ms after the turn.
  A later `-c analytics.enabled=true` in `args` wins (measured).
- **Instruction:** `-c developer_instructions=<instruction as a TOML basic string>` (`-c` parses
  its value as TOML; an instruction that parsed as another value would not be a string). It
  arrives as a `developer` message, never in the user's turn. Measured: once per request on
  every turn when passed every turn; a changed value on a resumed turn is not added (the stored
  one stays); with it passed on the first turn only, a compaction drops it from every later
  request, and passed on every turn it survives the compaction. So it is passed on every turn.
  It replaces any `developer_instructions` of the user's `config.toml` or profile (a `-c`
  override wins); `reply_instruction` is the place for the user's own text.
- **Permissions:** `codex exec` forces approval policy `never` and refuses every approval request
  (commands, patches, `request_user_input`), so a turn never waits. `auto` = `--approve-for-me`
  (workspace-write sandbox, network off; an escalation goes to the automatic reviewer, an extra
  model call), `bypass` = `--dangerously-bypass-approvals-and-sandbox`, `ask` = `--sandbox
  read-only` (reads and read-only commands run; writes are refused), explicit so a user's
  `sandbox_mode` cannot loosen it. `ask` matches OpenCode's: nothing changes without the user.
  In a git repo, every writable mode (`auto`, `bypass`, plain workspace-write) makes Codex write
  `[projects."<dir>"] trust_level = "trusted"` into `$CODEX_HOME/config.toml` (measured; read-only
  writes nothing): Codex's own behavior, documented for users.
- **Events:** `item.completed` of an `agent_message` → `Text { item.id, text }` (one message per
  item; the last is the reply, as Codex's own final message; an empty one is kept, so an empty
  final message is "ended the turn without a reply", never the acknowledgement before a tool
  call, which was spoken already); `item.started` of
  `command_execution`, `file_change`, `web_search`, `todo_list`, `collab_tool_call` → `Tool(type)`,
  and of `mcp_tool_call` → `Tool("mcp__<server>__<tool>")`, so the activity and the interim come
  when a call starts (file changes and web searches also start, measured); `turn.failed` →
  `Error(error.message)`, a provider's JSON body reduced to its `error.message` and a
  `401 Unauthorized` turned into "it is not signed in, or its sign-in expired ... `codex login`",
  or, when the child has a `CODEX_API_KEY` (Codex uses it before any saved sign-in; `codex exec`
  reads no other key variable, 0.164.0 source), "the API key in CODEX_API_KEY was refused".
  Nothing from top-level `error` lines and `error` items (retries and warnings; a fatal one
  comes back in `turn.failed`), from `item.completed` of tools (an unfinished one is completed
  after the final message at `turn.completed`, and would release the reply as an interim), nor
  from `item.updated`, `reasoning`, `turn.started`, `turn.completed`. The turn ends at the exit.
- **Stop:** SIGINT interrupts the turn; Codex exits (about 70 ms through whisrs) and ends the
  command it runs, sandboxed (`bwrap --die-with-parent`, its own session) or not; SIGKILL of the
  group also took the command along (measured). The next turn resumes the thread with the stopped
  turn in it (measured). Interrupting a command in the workspace-write sandbox leaves bwrap's
  empty mount points (`.git`, `.codex`, `.agents`, `.aws`) in a dir that lacked them (measured;
  a command that runs to its end leaves none). Codex's behavior; documented, not cleaned up.
- **Restart:** as OpenCode's: `PR_SET_PDEATHSIG` interrupts the turn when whisrsd exits, the
  next daemon sweeps its scope, the rewatch fails it with `cut_off` (measured), and the next turn
  resumes the thread.
- **Environment:** besides the base's stripping, what a parent Codex session sets in its tool
  shells (`CODEX_THREAD_ID`, `CODEX_SESSION_ID`, `CODEX_TOOL_CALL_ID`, `CODEX_CI`, `CODEX_SANDBOX`,
  `CODEX_SANDBOX_NETWORK_DISABLED`, `CODEX_VERSION`, `CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS`)
  and `RUST_LOG` (Codex reads it and logs to stderr at whisrsd's level, measured). `CODEX_HOME`
  and `CODEX_API_KEY` pass; the latter beats a ChatGPT sign-in, which the docs say.
- **List:** the whole thread id, what `codex resume <id>` takes.

### Copilot driver (verified on 1.0.103 against a local mock model)

Tested with the npm-packed `@github/copilot-linux-x64` 1.0.103 in offline BYOK mode
(`COPILOT_OFFLINE=true`, `COPILOT_PROVIDER_BASE_URL` = a scripted chat-completions server), through a
wrapper that also isolated `COPILOT_HOME`/`HOME`, emptied every GitHub token and config dir, made the
keyring unreachable and sent any other request to a refusing local proxy: no account, no GitHub
request (the proxy logged none), `premiumRequests: 0` on every run. With a GitHub account each turn
uses the user's Copilot plan's usage (AI credits, or premium requests on the legacy billing
platform, as `copilot help billing` puts it), per Copilot's own billing.

- **Every turn:** `copilot --output-format json --session-id=<id> <permissions> [args…]` for a new
  or pending session, `--resume=<id>` in its place once confirmed, the prompt on stdin: prompt mode
  reads stdin when there is no `-p` (verbatim, a leading `--`, `$HOME` and quotes as typed,
  measured), so the user's words are never in argv (`ps`). With `/dev/null` it exits 1 "No prompt
  provided". `--resume` and `--session-id` take `=`, since `--resume [<value>]` has an optional value.
  A prompt that starts with `/` runs a Copilot command instead (read in the bundle, not run):
  prompt mode passes it to `autopilotSlashCommandParse` (native) and `commands.invoke` (a slash
  command or a skill), and `cyo` runs `/compact` on the trimmed prompt, so a leading space or newline
  would not keep `/compact` from running. Not neutralized: whisrs passes the words as they are, and
  the docs say so.
- **Session id:** whisrs mints a version 4 UUID (`copilot::new_session_id`) and reports it at spawn,
  pending (see the base). `--session-id=<id>` starts a session with an id Copilot does not have and
  continues one it has (measured), so a pending session passes it again: a first turn stopped
  before Copilot made the session starts it, one that failed after continues it. `--resume=<id>`
  fails on a session Copilot does not have (stderr `No session, task, or name matched`, exit 1,
  `stderr_failure` → "Copilot no longer has this agent's session ... `--new`"), which is why a
  confirmed session uses it: never an empty session under the old id. Only a UUID is resumed
  (`--resume` also matches prefixes and names). `result.sessionId` names the session too.
  `--name` was not used: Copilot refuses it beside `--resume` and beside a `--session-id` it
  already has, which a pending retry may be (measured); a session is named after its first words.
  `--trust-folder` is refused beside either flag (measured), so it cannot be in `args`.
- **On every turn:** the permission flags and `args`: an `--agent` of turn 1 is gone on a resumed
  turn (contract 3.4), a `--model` stays the session's either way (measured), and permissions are
  per run.
- **Instruction:** `<data>/whisrs/copilot/<agent>/.github/instructions/whisrs-voice.instructions.md`
  (front matter `applyTo: "**"`, shown to the model as it is; 1.0.103 loads the file without it as
  well), written when it differs (`Copilot::write_instruction`, via a `.partial` rename), and its
  directory appended to `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` after the daemon's own entries on every
  turn (a path with a comma fails the turn). Measured: it lands in the system prompt, once per
  request, on a first and on a resumed turn, never in a user message; a turn without the variable
  has none; Copilot keeps only its system-prompt snapshot (`system.message` in its own
  `events.jsonl`), nothing in the conversation, so the user's own `copilot --resume` runs without
  it. An `AGENTS.md` or `copilot-instructions.md` in that directory is not loaded. Rejected:
  `--agent whisrs` (needs a file in the user's `~/.copilot/agents/`), prefixing the first prompt
  (shows as the user's words, summarized away by a compaction). A changed instruction applies from
  the next turn. `--no-custom-instructions` in `args` turns it off.
- **Permissions:** prompt mode never asks: a tool that needs an approval is denied (`code:
  "denied"`, "no interactive user response was available") and the model goes on, exit 0. Measured
  per mode with a shell call, a file create, a read in the dir and a read of `/etc/hostname`: no
  flags = shell and create denied, the read allowed, outside denied; `ask` = `--deny-tool=shell
  --deny-tool=write` = the same, by rule ("denied due to the following rules: `shell`"), and a deny
  rule wins over `--allow-all-tools` (so over the user's own allows too); `auto` =
  `--allow-all-tools` = shell and create run, outside still denied (URLs too); `bypass` = `--yolo` =
  everything. `ask` matches OpenCode's and Codex's: reads, no commands, no changes.
- **Events:** a sub-agent's event says nothing, whatever its type (`copilot::from_sub_agent`,
  Copilot's own `nl`/`ms` test: a non-empty top-level `agentId`, else `data.agentId`, else the
  deprecated `data.parentToolCallId`). `assistant.message` → `Text { data.messageId, data.content }`
  (an empty one too, so an empty last message is no reply), and, when it has `toolRequests`,
  `Tool(<first request>)` right after it, so a text written with a tool call is an interim at once;
  `tool.execution_start` → `Tool(data.toolName)` (MCP: `mcp__<mcpServerName>__<mcpToolName>`),
  deduplicated by the base; `session.error` as Copilot's own turn outcome counts it (`pyo` and
  `oQt` in 1.0.103's `cli-main.js`): `errorType` `"hook"` → nothing, `"model_call"` →
  `RecoverableError(data.message)` (fails the turn only when no main-agent message follows it;
  Copilot's prompt mode exits 0 after one either way), any other → `Error(data.message)` (a
  `remediation: "sign_in"` error gets "Run `copilot login` in a terminal"); a `session.warning` of
  `copilot::FAILING_WARNINGS` (`policy_blocked`, `compaction_static_context_blocked`, which Copilot
  exits 1 on) → `Error(data.message)`; `abort` → `Error("it was interrupted before it replied")`,
  since an interrupted turn prints `result` with `exitCode: 0` and exits 0; `result` →
  `Session(sessionId)`. Retries (`model.call_failure`, `assistant.turn_retry`, `session.info`) say
  nothing; any other `session.error` is the last word of a failed turn (exit 1). Errors measured: HTTP 400 (`"400 <provider message>"`), a
  BYOK 401 ("Authentication failed with provider at ... Check your COPILOT_PROVIDER_API_KEY ..."),
  a provider that is down ("Could not connect to local model provider at ..."), all kept as
  Copilot's words. Not signed in (stderr "Error: No authentication information found.", exit 1)
  was not run, since it needs Copilot without BYOK: the text is 1.0.103's `cli-main.js`;
  `stderr_failure` turns it into "Copilot is not signed in to GitHub. Run `copilot login` ...".
- **Activities:** Copilot's tools (`bash` and its `read_bash`/`write_bash`/`stop_bash`/`list_bash`,
  `view`, `create`, `edit`, `grep`, `glob`, `web_fetch`, `task` and its `read_agent`/`write_agent`/
  `list_agents`) are mapped in `activity_for_tool`; `sql` (its todo table and more) reads "Working".
- **Stop:** SIGINT interrupts the turn (`abort`, `result`, exit 0) and Copilot ends the tool's shell.
  The shell runs in a session of its own and survives a SIGKILL of Copilot's group (contract 3.6), which the
  scope and the tree reach. Measured through whisrs: `--stop` during a shell `sleep` returned in
  87 ms with no `copilot` or `sleep` process and no scope left; a SIGTERM or SIGKILL of the daemon
  mid-turn left none either, the restarted daemon said "whisrs restarted during that turn, which
  stopped it before Copilot replied", and the next turn continued the session (the model saw every
  earlier turn).
- **Environment:** besides the base's stripping, `COPILOT_ALLOW_ALL` (truthy approves every tool;
  exactly `true` also trusts the folder and runs its hooks) and what a parent Copilot session sets in
  its tool shells: `COPILOT_CLI`, `COPILOT_AGENT_SESSION_ID`, `COPILOT_CLI_BINARY_VERSION`,
  `COPILOT_CLI_RESOLVED_DIST_DIR`, `COPILOT_LOADER_PID` (measured on 1.0.103; an inherited
  `COPILOT_LOADER_PID` reached the tools unchanged), plus `COPILOT_PROMPT_TURN_DELIMITER`, which
  splits a prompt into several (`nyo` in the 1.0.103 bundle). Its `GIT_*` (`GIT_ASKPASS`, `GIT_EDITOR`,
  `GIT_TERMINAL_PROMPT`, `GIT_OPTIONAL_LOCKS`, `GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n`) pass: Copilot
  sets them again for its tools (an inherited `GIT_CONFIG_COUNT` is appended to, measured), and a
  user's own `GIT_CONFIG_*` would go with them. `COPILOT_HOME`, the tokens (`COPILOT_GITHUB_TOKEN` >
  `GH_TOKEN` > `GITHUB_TOKEN` > the stored login), `COPILOT_OFFLINE`, `COPILOT_MODEL` and
  `COPILOT_PROVIDER_*` pass. `RUST_LOG` changes nothing in Copilot's output (measured), so it passes.
- **Not done:** `--no-auto-update` (it would run an older Copilot than the user's own; the JSON shape
  can change with an update), `--no-ask-user` (prompt mode has no `ask_user` tool), `--acp` (the
  route to voice approvals, "Later").
- **List:** the whole session id, what `copilot --resume=<id>` takes.

## Later

- Approving a permission prompt by voice mid-turn, via ACP (Agent Client Protocol).

## Acceptance criteria (phase 1c: overlay and feedback)

1. No `[[agents]]`: no agent overlay thread or surface, no tailer; pill and tray as before.
2. During an agent's recording, transcription and every utterance of it the pill stays
   hidden and the tray Idle; media pause and hooks still fire for the agent recording.
3. Dictation, command mode, `[[llm_commands]]`, read-aloud and `speak --text` drive the
   pill and tray unchanged; a plain toggle on an agent recording shows the pill at
   Transcribing.
4. The overlay defaults to bottom right and works in all four corners; orb and face draw
   all seven states in the palette; the card shows name, state, activity and timer; rows
   stack from the corner, hidden when none; done and error fade about 3 s after the reply.
5. Hidden costs nothing: no frame requests or commits (Wayland), no redraws (X11).
6. summary speaks only the reply; brief one acknowledgement per turn (only when a tool
   call followed it); conversational adds updates at least 20 s apart, deduplicated. The
   reply is spoken exactly once and never dropped.
7. Unknown `feedback`, `style` or `corner`: one warning and the default; the new keys raise
   no unknown-key warning; `whisrs config` asks style and corner only with agents.
8. GNOME Wayland: no native surface attempted, `AgentsChanged` broadcast, agent toasts
   stay notifications.

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
