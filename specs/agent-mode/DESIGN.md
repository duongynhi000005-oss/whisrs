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
harness = "claude"               # claude (phase 1); opencode | codex | copilot (phase 2)
dir = "~/Projects/whisrs"        # required, ~ expanded, must exist
permissions = "auto"             # auto (default) | bypass | ask
command = "claude"               # optional binary path; default: the harness name on PATH
args = ["--remote-control", "jarvis"]   # optional, passed on the first turn of a session
reply_instruction = "..."        # optional, replaces the built-in voice instruction
```

- `permissions` is harness-neutral and mapped per driver. No warning for `bypass`: the user
  chose it. Claude: `auto` → `--permission-mode auto`, `bypass` → `--permission-mode
  bypassPermissions`, `ask` → no flag.
- Validation warns (and skips the entry) on a missing/duplicate name, unknown harness or
  missing dir; a hotkey clash is reported like the `[[llm_commands]]` clashes. Agents with
  `[tts] enabled = false` get one warning: replies arrive as notifications only.
- `command` exists because a systemd user unit often lacks `~/.local/bin` on PATH. A spawn
  failure names it.

## CLI and IPC

| CLI | IPC |
|---|---|
| `whisrs agent` (list: name, harness, idle/working/needs-you, short session id) | `{"cmd":"agent","action":"list"}` → `Response::Agents` |
| `whisrs agent <name>` (voice turn; same as the hotkey) | `{"cmd":"agent","action":"toggle","name":"…"}` |
| `whisrs agent <name> --text "…"` (typed turn) | `{"cmd":"agent","action":"send","name":"…","text":"…"}` |
| `whisrs agent <name> --new [--text "…"]` (forget the session; with text, start a new one) | `"action":"reset"` / `"send"` + `"new":true` |
| `whisrs agent <name> --stop` (stop the running turn, keep the session) | `"action":"stop"` |
| `whisrs speak --text "…"` / `--text -` (stdin) | `{"cmd":"speak-text","text":"…"}` |
| `whisrs agent-hook` (hidden; harness hook → daemon) | `{"cmd":"agent-event","payload":{…}}` |

- `speak-text` is its own variant, not a field on `speak`: an older daemon must answer it
  with an error, never read the selection instead.
- `agent-hook` reads the hook JSON on stdin, forwards it, and always exits 0 within two
  seconds, even when the daemon is down. It never blocks the harness.
- The CLI stays a thin client. All logic is in the daemon.

## Turn flow

1. **Toggle while Idle:** if this agent has a turn running, refuse with a short spoken and
   notified "Still working on the last one." Otherwise record exactly like command mode's
   instruction recording (overlay, cues, both streaming and batch paths), with the agent as
   the sink instead of the LLM rewrite. No selection capture. An empty transcript ends
   quietly with a notification.
2. **Toggle while Recording for this agent:** stop and transcribe. Recording for anything
   else: refuse as busy, like command mode.
3. **Toggle while Speaking:** stop playback and start recording for this agent (barge-in, so
   answering the agent's question is one press).
4. **Transcript ready:** start the turn on a spawned task. The state machine returns to Idle
   at once; dictation keeps working while the agent runs.
5. **Turn ends:** reply text → spoken text (below) → spoken when the daemon is Idle (a FIFO
   of pending replies, drained by watching the state channel like `hook_dispatch_loop`) and
   shown as a notification when `[general] notify` is on. Errors always notify.
6. Several agents can run turns at once. Only one recording exists at a time.

## Claude Code driver (verified on 2.1.301)

- **Environment:** inherit the daemon's environment minus what a parent Claude session sets:
  `CLAUDECODE`, `CLAUDE_CODE_*`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `CLAUDE_PROJECT_DIR`,
  `CLAUDE_JOB_DIR`, `AI_AGENT`. Keep `CLAUDE_CONFIG_DIR`. cwd = `dir`.
- **First turn:** `claude --bg --name whisrs-<agent> --permission-mode <m>
  --append-system-prompt <instruction> --settings <hooks json> [args…] "<text>"`.
  Prefer `--session-id <uuid>` so the session id is known up front (verify it works with
  `--bg`); otherwise read it from `claude agents --json --all` by the bg id that `--bg`
  prints (`backgrounded · <id>`). The appended prompt is recorded on the first request and
  reused on every resume (`--system-prompt-snapshot` defaults to on), so it is passed once.
- **Hooks json:** `Stop` and `Notification` → `<abs path of whisrs> agent-hook`, short
  timeout. Passed with `--settings`, so only whisrs-started sessions have them; the user's
  own settings are untouched.
- **Next turn:** if the bg process is still alive, `claude stop <bg id>`, then `claude --bg
  --resume <session id> "<text>"`. It keeps the session id and the saved options
  (`--remote-control`, `--settings`). If the output says it started a copy, record the copy's
  session id and log it.
- **Turn end:** the `Stop` event whose `session_id` matches carries `last_assistant_message`
  and `transcript_path`. A `Notification` for a permission prompt marks the agent
  needs-you and speaks a short notice (for example "Waiting for your approval to use Bash");
  ignore idle notifications. A voice turn while needs-you is sent normally (stop + resume).
- **Fallback poll** while a turn runs, every ten seconds: `claude agents --json --all`, our
  bg id only. `done` without a Stop event → last assistant text from the transcript JSONL.
  Gone or exited → error "the session ended". `blocked` → needs-you.
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

- Take the last `<spoken>…</spoken>`. Without one, take the whole reply and keep its first
  two sentences.
- Sanitize: drop fenced code; inline code of more than three words dropped, shorter kept as
  words; `[text](url)` → text; bare URLs and file paths dropped; markdown markers, list
  bullets and table rows stripped; whitespace collapsed.
- Cap at 400 characters on a sentence boundary, so every TTS backend takes it in one request.
- Empty after all that → "Done."
- Pure functions in `src/agent/spoken.rs`, unit tested.

## State and persistence

- `src/daemon/agent.rs` owns an agent runtime in `DaemonContext` behind its own lock, outside
  `DaemonState`. The state machine only gains the agent recording sink.
- `$XDG_DATA_HOME/whisrs/agents.json` (0600): per agent `harness`, `dir`, `session_id`,
  `bg_id`, `transcript_path`, `turn_running`, `updated_at`. Written on every change. At
  startup an agent with `turn_running` is watched again (poll), so a reply that lands after a
  daemon restart is still spoken. A changed `harness` or `dir` starts a fresh session.
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
7. `--stop`, `--new` and `whisrs agent` work. A turn in flight across a daemon restart is
   still spoken after it.
8. Missing binary, untrusted dir and a vanished session each give an actionable notification
   and leave the agent usable.
9. Sessions whisrs did not start are never stopped, resumed or read.
10. Docs: `docs/configuration.md`, README, `contrib/whisrs.1`, CLAUDE.md (structure, IPC,
    running).
11. The full CI gate in CLAUDE.md passes.
