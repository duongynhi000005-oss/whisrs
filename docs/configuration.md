# Configuration

Config file: `~/.config/whisrs/config.toml` (permissions: `0600`).

The interactive `whisrs setup` will write a working file for you. The reference below documents every section.

## Full config reference

```toml
[general]
backend = "groq"            # groq | deepgram-streaming | deepgram | openai-realtime | openai | openai-compatible-realtime | local-whisper | asr-sidecar
language = "en"             # ISO 639-1 or "auto"
silence_timeout_ms = 2000   # auto-stop after silence (streaming only)
notify = true               # desktop notifications
history = true              # save dictations to ~/.local/share/whisrs/history.jsonl for
                            # `whisrs log`. false stops new writes only: existing
                            # entries stay until `whisrs log --clear`
remove_filler_words = true  # strip "um", "uh", "you know", etc.
filler_words = []           # custom list (empty = use built-in defaults)
audio_feedback = true       # play tones on record start/stop/done
audio_feedback_volume = 0.5 # 0.0 to 1.0
vocabulary = ["whisrs", "Hyprland"]  # custom terms for better transcription accuracy
                            # (sent to Deepgram as keyterms on Nova-3/Flux, ignored on
                            # older models; elsewhere it is folded into the prompt hint,
                            # so it only reaches the backends that send one — see prompt)
                            # very long lists are truncated for Deepgram, since every
                            # keyterm rides in the request URI — the daemon warns at
                            # startup naming how many terms actually reach it
                            # more terms can come from an optional vocabulary.txt next
                            # to this file — see "The vocabulary file" below
prompt = "Speech is in English or Spanish. Transcribe in the language spoken; never translate."
                            # optional sentence-style context, prepended to vocabulary
                            # (reaches Groq, OpenAI REST, local whisper.cpp, and the
                            # asr-sidecar, which takes it as its `hotwords` field.
                            # Deepgram sends no prompt — use vocabulary there instead.
                            # openai-compatible-realtime sends neither, so neither key
                            # reaches it. openai-realtime sends a prompt with
                            # gpt-live-transcribe and gpt-4o-transcribe, but not
                            # with gpt-realtime-whisper)
                            # `whisrsd` warns at startup if either key targets a backend
                            # that discards it. `whisrs config` warns the same on save.
                            # Recordings (whisrs recording) never send the prompt,
                            # only the vocabulary (and with local-whisper, neither).
                            # local-whisper sends it with every dictation phrase or
                            # window it decodes, so write it in the language you
                            # dictate in: a prompt in another language pulls the
                            # transcript toward that language.
tray = true                 # system tray icon (requires SNI host like waybar)
overlay = false             # on-screen dictation overlay (Hyprland/Sway, GNOME extension)

# Run every dictation through the [llm] backend before it is typed.
# Default: false. This is the always-on flavor of [[llm_commands]] below:
# same rewrite, but on the normal `whisrs toggle` key instead of a dedicated
# hotkey per entry. Needs an [llm] section (or WHISRS_OPENAI_API_KEY /
# WHISRS_GROQ_API_KEY).
#
# Batch backends only: deepgram, groq, openai, asr-sidecar. The streaming
# backends, which are deepgram-streaming, openai-realtime,
# openai-compatible-realtime AND local-whisper, type text as it arrives, so
# there is never a whole transcript to post-process and the flag does nothing
# at all. local-whisper is the one to watch: it runs offline, but dictation
# with it streams by default, so llm_post_process is a silent no-op there too.
# Set [local-whisper] segmentation = "none" to make it a batch backend.
# `whisrsd` warns at startup if you pair a streaming backend with this flag.
# Or use an [[llm_commands]] hotkey instead, which works whatever the backend.
#
# If the LLM call fails, times out (30s), or returns nothing, the raw
# transcript is typed instead, so a dictation is never lost to post-processing.
# A post-processed dictation is logged as <backend>+llm (e.g. "groq+llm") in
# `whisrs log`; one that fell back to the raw transcript keeps the plain
# backend name.
llm_post_process = false
# Instruction applied to the transcript when llm_post_process is on. This is
# NOT `prompt` above: that one is a hint for the transcription backend and
# never reaches the LLM. Defaults to the conservative cleanup pass below, so
# `llm_post_process = true` alone already does something sensible.
llm_instruction = "Fix punctuation, capitalization and obvious transcription errors in the following text. Keep the wording and the meaning unchanged. Return only the corrected text, with no explanations and no quotes."

# Optional — controls overlay appearance when enabled.
# Defaults to a 100×40 pill with the "carbon" theme.
# When the overlay is on, recording/transcribing toast notifications are
# auto-suppressed (errors still pop) so the same event isn't double-signaled.
[overlay]
theme = "carbon"            # "carbon" (default) | "ember" | "cyan" | "custom"
width = 100                 # 90..=120 (clamped)
height = 40                 # 36..=48 (clamped)
# Where the pill sits: "bottom-center" (default) | "bottom-left" |
# "bottom-right" | "top-left" | "top-center" | "top-right".
# "-middle" works as a synonym for "-center". 16 px from the screen edges.
# On GNOME this needs the Shell extension from this release. An older
# one ignores the key and stays at the bottom center, and `whisrs setup`
# does not replace an installed extension, so update it by hand
# (contrib/gnome-shell-extension/README.md) and log out and back in.
position = "bottom-center"

# When theme = "custom", these override the named theme. Hex strings:
# #RGB, #RRGGBB, or #RRGGBBAA. Anything missing falls back to carbon.
# The transcribing color is reused for the read-aloud synthesizing sweep;
# speaking overrides the read-aloud audio-reactive bar color.
# [overlay.colors]
# background   = "#0E0E10EB"
# ring         = "#3A3A4050"
# recording    = "#F0EDF5"
# transcribing = "#9CA3AF"
# speaking     = "#5EEAD4"
# glow         = "#F0EDF5"

[audio]
device = "default"         # input device; see "Choosing the microphone" below

[input]
# Inter-key delay for the virtual keyboard. Raise this if a TUI
# drops characters while whisrs is typing — e.g. Node/Ink-based apps like
# Claude Code in raw mode. The uinput backend treats 0 as 1 (with no delay
# the kernel's input queue overflows and the end of the text is lost); the
# xtest backend honours 0. Default: 2.
key_delay_ms = 2
# Keyboard-injection backend: auto | uinput | wayland-vk | xtest.
# auto uses the Wayland virtual keyboard when the compositor supports it,
# otherwise uinput. Default: auto.
# On X11, set "xtest" to type through the X server's XTEST extension instead
# of uinput: no /dev/uinput access needed, and key_delay_ms = 0 drops no
# keys. auto never picks it. It refuses to start on Xwayland, and when the X
# server's XTEST keyboard has a different layout than the one whisrs detected
# (a layout set only in xorg.conf, e.g. by `localectl set-x11-keymap`, does
# not reach that keyboard; run setxkbmap with your layout to fix that), or
# when an XKB option moves Ctrl or Shift (e.g. ctrl:swapcaps; use auto).
backend = "auto"
# How long to wait, in milliseconds, for physically held modifier keys (Super,
# Alt, Ctrl, Shift) to be released before whisrs sends a keystroke with a
# batch backend (groq, openai, deepgram, local-whisper, asr-sidecar) or in
# command mode: typing, the paste shortcut, command mode's line clear and the
# selection copy. A key typed while a modifier is held becomes a shortcut, so
# stopping a dictation with Super+W and keeping Super down would otherwise
# fire Super+<letter> compositor binds. whisrs never types while a modifier
# is held, and checks before every key: pressing one while the text is being
# typed pauses the typing until it is released. If one is still down when
# this runs out, typing stops there: the rest of the text (all of it if
# nothing was typed yet) is copied to the clipboard and a warning is logged
# (paste it with Ctrl+V). Text already typed stays where it is. In command
# mode at a terminal, a modifier held from the start keeps the line from
# being cleared too. 0 = don't wait: if a modifier is held, the rest of the
# text goes straight to the clipboard.
# Streaming backends (deepgram-streaming, openai-realtime,
# openai-compatible-realtime) ignore this setting: they also pause before
# any key while a modifier is held, wait for the release however long it
# takes, and then type the waiting text in order. `whisrs cancel` ends that
# wait and discards the text not typed yet.
# The check reads /dev/input; when it can't, it can't see held keys and keys
# are sent as before. Default: 10000.
modifier_wait_ms = 10000
# Inject text by clipboard paste (Ctrl+V) instead of typing keystrokes.
# Default: false.
#
# The uinput backend emits raw keycodes that the compositor decodes through
# the target window's ACTIVE XKB layout. On compositors without the Wayland
# virtual-keyboard protocol (e.g. KWin), when that active layout differs from
# the one whisrs detected — typically with per-window layouts (KDE
# SwitchMode=WinClass) or a non-US keymap — output is garbled (z<->y, mangled
# punctuation, dropped accents/umlauts). Pasting goes through the clipboard,
# which is layout-independent and Unicode-complete, so text comes out verbatim.
#
# Trade-offs: briefly replaces the clipboard (restored right after) and the
# target app must support Ctrl+V (terminals get Ctrl+Shift+V). It covers batch
# (non-streaming) dictation and command-mode output (`whisrs command` injects its
# LLM result with a single injection call, so it honors this whatever the
# backend).
# The streaming dictation path is the exception: streaming backends (including
# local-whisper, unless its `segmentation` is "none") type incrementally and
# ignore it. `whisrsd` warns at startup if paste is set
# with one of those backends.
paste = false
# Leave the final transcript in the system clipboard in addition to
# injecting it at the cursor — a fallback if injection fails silently or
# produces garbled text, so you can paste and fix manually. Default: false.
#
# With paste mode (`paste = true`) the clipboard is normally restored to its
# previous content right after pasting; with clipboard_fallback set, that
# restore is skipped and the transcribed text stays in the clipboard. In
# typing mode (default) the text is copied to the clipboard after typing,
# except when a held modifier stopped the typing (see modifier_wait_ms): then
# the clipboard holds only the untyped rest, so pasting it does not repeat
# the typed part.
# Streaming backends (deepgram-streaming, openai-realtime, local-whisper, ...)
# type incrementally and copy the full final transcript when recording stops.
# `whisrs cancel` never copies anything, on any backend — cancel discards.
clipboard_fallback = false
# Copy-only mode: the final transcript is written to the clipboard and never
# injected at the cursor — no keystrokes, no Ctrl+V. Overrides paste and
# clipboard_fallback (both become no-ops). Handy for a "dictate to clipboard"
# workflow. Default: false.
#
# This also covers the reply of `whisrs command` and `[[llm_commands]]`. A
# multi-line reply is copied even with a terminal focused: nothing is typed,
# so it is not refused (see "What happens to the LLM's reply").
clipboard_only = false
# Extra window classes to treat as terminal emulators, checked alongside the
# built-in list. Default: [] (built-in list only).
#
# WARNING: terminal detection is what makes command mode clear the prompt line
# with Ctrl+A then Ctrl+K before injecting its result. If you list a class that
# is not a terminal, that Ctrl+A / Ctrl+K goes into an ordinary text field and
# empties it. Terminal detection also swaps Ctrl+C for Ctrl+Shift+C in the
# selection path, so a mis-listed class can instead open DevTools in browsers
# and Electron apps. Only add classes you have confirmed belong to a terminal.
#
# The config is read once at daemon startup, so run `whisrs restart` after
# changing this key or the new value has no effect.
#
# To debug a class that will not match, run `RUST_LOG=debug whisrsd` and look
# for the `is_terminal_class(...)` line for that window. It names the class
# under test, the verdict, and (on a match) which stage matched: built-in
# whole identifier, your terminal_classes entry, or a built-in leaf name.
#
# Use this for the two cases the built-in list cannot cover:
#   * an st build with a custom `termname` in config.h, which reports that name
#     as its class instead of st-256color
#   * scratchpad / dropdown windows launched under a renamed class, such as
#     Alacritty-float, kitty-dropdown or wezterm-quake
#
# Entries are compared case-insensitively against the WHOLE window class, never
# as substrings, so "st" matches a window whose class is exactly "st" and not
# "steam". Unlike the built-in list, they are not matched by their last
# dot-segment either: listing "warp" matches the class "warp" and leaves
# "app.drey.Warp" (GNOME's Magic Wormhole client, not a terminal) alone. To
# match a reverse-DNS app_id, write it out in full.
#
# Read the class off your compositor:
#   hyprctl activewindow | grep class
#   niri msg focused-window
#   swaymsg -t get_tree   (the focused node's app_id for Wayland apps,
#                          window_properties.class for XWayland ones, falling
#                          back to window_properties.instance)
#   xprop WM_CLASS        (then click the window; the SECOND string is the
#                          class, and that is the one whisrs reports; when it
#                          is empty, whisrs falls back to the first string)
#
# A class is reported on Hyprland, Niri, Sway and X11. GNOME reports none
# without a shell extension (issue #72), and KDE reports none because whisrs
# does not speak the org_kde_plasma_window_management Wayland protocol (issue
# #127), so this key has no effect on either.
#
# Sway and X11 started reporting a class in issue #71, so terminal detection
# now fires on those sessions. Four behaviors change there:
#   * command mode clears the prompt line with Ctrl+A then Ctrl+K before
#     injecting. That is right at a shell prompt and wrong inside a TUI; GNU
#     screen takes Ctrl+A as its prefix, and many tmux users rebind theirs to
#     match.
#   * a multi-line LLM reply is refused at a terminal and kept in `whisrs log`
#     instead of being typed (discarded with `[general] history = false`),
#     including inside a terminal-hosted editor.
#   * the selection copy fallback sends Ctrl+Shift+C instead of Ctrl+C. Stock
#     xterm and urxvt do not bind Ctrl+Shift+C. The primary selection is tried
#     first and covers a highlighted selection, so this affects the fallback
#     only.
#   * with `paste = true`, injection sends Ctrl+Shift+V instead of Ctrl+V. This
#     one is a fix, not a loss: Ctrl+V at a terminal is readline quoted-insert.
# With `clipboard_only = true` only the third applies: nothing is typed or
# pasted, so there is no line clear, no refusal and no Ctrl+Shift+V.
terminal_classes = []

[groq]
api_key = "gsk_..."
model = "whisper-large-v3-turbo"

[deepgram]
api_key = "..."
model = "nova-3"

[openai]
api_key = "sk-..."
model = "gpt-4o-mini-transcribe"

# For backend = "openai-realtime", select model = "gpt-live-transcribe".
# languages = ["ru", "en"]  # expected input languages for mixed-language dictation

# gpt-live-transcribe streams text as you speak and commits the final transcript
# when recording stops. languages are hints, not an output-language guarantee.
# Precedence for this model: a per-session `whisrs toggle -l <code>`, then
# languages, then [general] language sent as a one-item list, then no hint when
# it is "auto". A -l equal to [general] language counts as no override and keeps
# the list. On any other model or backend, languages is ignored and `whisrsd`
# warns at startup. OpenAI also accepts some three-letter and regional codes.

# External OpenAI-compatible realtime server (WebSocket).
# Use this for Lemonade-style services that speak the OpenAI Realtime
# transcription event model over WebSocket instead of HTTP.
# In the current whisrs typing pipeline, replaceable interim partials are kept
# internal and only completed phrases are typed at the cursor.
# Note (Lemonade): load the model server-side (not just `pull`) or it returns empty text.
[openai-compatible-realtime]
url = "ws://localhost:12345/realtime"
model = "Whisper-Tiny"
profile = "lemonade"        # currently only "lemonade"
turn_detection = "server-vad"  # recommended; see notes below
# api_key = "optional bearer token"

# turn_detection:
# - "server-vad" (recommended): the server watches for pauses in your speech
#   and sends completed phrases as you talk. Choose this if you want text to
#   appear a phrase at a time during a longer dictation session.
# - "manual-commit": the server waits until whisrs stops recording before it
#   flushes the final phrase. Choose this if your server's pause detection is
#   too eager, or if you would rather get one result only when you stop.
#
# In both modes, whisrs only types completed text for this backend. It does
# not type unstable partial hypotheses as they change.

[local-whisper]
model_path = "~/.local/share/whisrs/models/ggml-base.en.bin"
# segmentation: how streaming audio is split before decoding.
# - "silence" (default): split into phrases at natural pauses and decode each
#   phrase exactly once. No overlap, no dedup — prevents repeated/invented
#   text. Continuous speech is force-split at the first silent moment after
#   20 s (hard ceiling 28 s) so it still emits.
# - "window": legacy 8s/2s overlapping sliding window with text-based dedup.
# - "none": no streaming. Nothing is typed while you talk; when you stop,
#   the whole dictation is transcribed and typed at once. Use it for long
#   dictation you don't want split into phrases.
#   The wait after stopping grows with the dictation (a few seconds per
#   minute of speech with large-v3-turbo on a GPU, longer on CPU). It also
#   makes [general] llm_post_process and [input] paste apply to dictation.
#   Past 28 s the dictation is decoded in windows of at most 28 s, cut at
#   pauses (or at 28 s in continuous speech) and each given the prompt, so a
#   long dictation does not fall into whisper's repetition loop (issue #208).
#   Command mode and [[llm_commands]] always take this batch path, whatever
#   segmentation says. The recording feature (whisrs recording) ignores this
#   setting and always decodes in phrases.
# segmentation = "silence"
# phrase_silence_ms: continuous silence (ms) that ends a phrase in "silence"
# mode; "window" streaming ignores it. In every mode it also finds the pauses
# where a batch call over 28 s is cut ("none" dictation, command mode,
# [[llm_commands]]) and where a recording is cut, capped at 900 ms for both
# (for a recording, so every gap between the speech it sends ends a phrase).
# Lower = snappier output, higher = fewer mid-sentence splits.
# phrase_silence_ms = 400
# flash_attn: enable whisper.cpp flash attention (default false). It can cut
# decode time on GPU builds (cuda), most on long recordings, and may change
# the output slightly. Little to gain on CPU. Takes effect when the daemon
# loads the model, so restart it after changing this.
# flash_attn = false

# Generic local ASR sidecar — talks to a small HTTP service that hosts the
# model (Moonshine, NVIDIA Parakeet, Microsoft VibeVoice-ASR, …). Keeps
# Python/PyTorch out of the Rust daemon. See contrib/asr-sidecars/ for
# ready-to-run sidecars and the wire-format contract.
#
# The contract is the OpenAI /v1/audio/transcriptions shape (multipart
# file/model/language in, {"text": ...} back), so this is also the backend for
# any OpenAI-compatible transcription endpoint — LiteLLM, Speaches, or a server
# you wrote yourself. The path is whatever you configure; /transcribe is only
# the default:
#   [asr-sidecar]
#   url = "http://127.0.0.1:8000/v1/audio/transcriptions"
#   model = "<model the endpoint serves>"
# One divergence from OpenAI: [general] prompt rides as the `hotwords` field
# (see prompt above), so a strict OpenAI-compatible server ignores it.
[asr-sidecar]
url = "http://127.0.0.1:8765/transcribe"
model = "microsoft/VibeVoice-ASR-HF"
# Optional bearer token. Only needed for endpoints that require auth (a hosted
# gateway, or a sidecar behind an authenticating proxy); local sidecars need
# none. WHISRS_ASR_SIDECAR_API_KEY overrides it.
# api_key = "optional bearer token"

# Command mode: LLM for voice-driven text rewriting.
# Also used by [[llm_commands]] (see below) and by
# [general] llm_post_process. Any OpenAI-compatible
# /chat/completions endpoint works here, including a local server:
#   [llm]
#   api_key = "not-needed"   # local servers don't validate this
#   model = "<model loaded in LM Studio / Ollama / llama.cpp server>"
#   api_url = "http://localhost:1234/v1/chat/completions"  # LM Studio default
[llm]
api_key = "sk-..."
model = "gpt-4o-mini"
api_url = "https://api.openai.com/v1/chat/completions"

# Named custom LLM commands: each gets its own hotkey. Dictate, the LLM
# applies the instruction to the transcribed text, result is typed at the
# cursor. A toggle-recording flavor of plain dictation — unlike command mode,
# there's no text selection involved. Uses the [llm] config above.
# Optional; can also be triggered via `whisrs llm-command <name>` for
# compositor keybind integration (same pattern as `whisrs toggle`).
# For one instruction applied to every dictation with no extra hotkey, use
# [general] llm_post_process instead.
#
# set_hotkey (optional): reprogram this command by selection. Highlight the new
# instruction text anywhere, press set_hotkey, and it becomes the command's
# instruction — saved back to this file and applied immediately (no restart).
# Lets you repurpose a command (translate -> summarize -> ...) without editing
# config. Also available as `whisrs llm-command-set <name>` for compositor
# binds. Selection-based on purpose: the instruction is exact, no dictation
# glitches.
#
# Tip: write the instruction so it clearly refers to the dictated text (e.g.
# "the following text") and pins the output ("Return only ..., no explanations,
# no quotes"). Small local models otherwise sometimes echo the instruction or
# add commentary.
[[llm_commands]]
name = "translate-de"
hotkey = "Super+Shift+T"           # run: dictate -> LLM -> type
set_hotkey = "Super+Shift+Alt+T"   # reprogram: select new instruction, press
instruction = "Translate the following text into German, using the friendly informal 'du' form and a warm, casual tone. Return only the translated text — no explanations, no quotes."

# Text-to-speech: read the current selection aloud (`whisrs speak`).
# Opt-in. model/voice are optional; each backend has its own default,
# so switching `backend` works without re-editing model/voice.
[tts]
enabled = false             # off by default
backend = "groq"            # groq | openai | deepgram | tts-sidecar
# model = "..."             # optional; backend default when unset.
#                           #   groq: canopylabs/orpheus-v1-english, openai: gpt-4o-mini-tts,
#                           #   deepgram: aura-2-thalia-en, tts-sidecar: kokoro
# voice = "..."             # optional; backend default (groq: autumn, openai: alloy,
#                           #   tts-sidecar: af_heart). Ignored by deepgram (voice is in the model id).
response_format = "wav"     # audio format requested from the API
# api_key = "..."           # optional; falls back to the backend's transcription key
#                           #   ([groq]/[openai]/[deepgram]). tts-sidecar needs none.
# url = "http://127.0.0.1:8880/v1/audio/speech"  # tts-sidecar only: local
#                           #   OpenAI-compatible server (Kokoro, Supertonic, ...)

# Recording (optional). Records your mic and the system audio as two tracks,
# then transcribes them after you stop. Every key is optional; see "Recording"
# below for how it works.
[recording]
# backend = "groq"            # default: [general] backend. A backend that can transcribe
#                             #   a recorded file: groq | openai | deepgram | asr-sidecar |
#                             #   local-whisper.
#                             #   deepgram-streaming uses Deepgram's REST API here.
#                             #   openai-realtime and openai-compatible-realtime are rejected.
# user_name = "Ada"           # your label in transcripts; default: your full name
#                             #   from the system account, else $USER
# language = "de"             # language of both tracks; default: [general] language.
#                             #   An ISO 639 code, optionally with a region, or "auto"
# mic_language = "de"         # language of your mic track; default: language
# system_language = "auto"    # language of the system audio track; default: language
# mic_device = "alsa_input.usb-..."  # a source name from whisrs config or pactl list short sources
#                             #   default: [audio] device
# system_device = "default"   # "default" = monitor of the default output,
#                             #   "none" = mic only, or a source name ending in .monitor
max_duration_minutes = 180    # recording stops by itself after this long (1 to 2160)
audio_retention_days = 7      # 0 = delete audio once transcribed, negative = keep forever
# dir = "~/Recordings"        # default: ~/.local/share/whisrs/recordings
# diarization = "auto"        # tell apart the voices on the system audio (see "Speakers"
#                             #   below): "auto" (the sidecar below when set, else
#                             #   Deepgram), "backend", "sidecar" or "off"
# diarization_url = "http://127.0.0.1:8770/diarize"  # a diarization sidecar (local server)
# num_speakers = 0            # voices the sidecar should find; 0 = let it decide (0 to 20)
# shared_mic = false          # several people share your mic: tell its voices apart too
# name_speakers = false       # ask the [llm] who the unnamed voices are; sends excerpts
#                             #   of each transcript to it (see "Naming the speakers")
# summary = false             # summarize each transcript with the [llm], and title a
#                             #   recording you gave no title; sends the whole transcript
#                             #   to it (see "Summaries and titles")
# summary_instruction = "..."  # your own rules for a summary, in place of the default ones

# Built-in global hotkeys (optional, works without WM keybinds)
# Triggers: A-Z, 0-9, F1-F24, space, enter, escape, tab, backspace, delete,
#   insert, home, end, pageup, pagedown, up, down, left, right, scrolllock,
#   pause.
# Modifiers: Super, Alt, Ctrl, Shift. At least one is required, except for
#   ScrollLock, Pause and F13-F24, which can be bound alone ("ScrollLock").
#   A bare binding fires only when no modifier is held. The modifier set must
#   match exactly, so "Ctrl+Alt+Ins" does not fire while Shift is also held.
#   Keys are not grabbed: the press still reaches the system, so ScrollLock
#   still toggles its LED.
# Two bindings sharing a combo both fire on one press, so whisrs warns at
#   startup when it finds a duplicate across [hotkeys] and [[llm_commands]].
[hotkeys]
toggle = "Super+Shift+W"
cancel = "Super+Shift+D"
command = "Super+Shift+G"
speak = "Super+Shift+R"
recording = "Super+Shift+M"   # start or stop a recording (mic + system audio)

# Dictation-lifecycle hooks: pause playing MPRIS media while dictating, run
# shell commands fire-and-forget on dictation start/stop.  The child inherits the
# daemon's environment and stdout/stderr (journal under systemd --user).
# A recording (whisrs recording) fires none of them.
[hooks]
media_auto_pause = true        # pause MPRIS players that are playing, resume those on stop
# on_record_start = ""         # shell command on dictation start
# on_record_stop = ""          # shell command on dictation stop
```

`media_auto_pause` only touches players that report `PlaybackStatus = "Playing"`
when a dictation starts, and resumes exactly those when it stops. A tab you paused
yourself stays paused. If a player's bus name changes mid-session (a browser
switching media sessions) its resume is skipped rather than guessed at.

To see what a hook printed, filter the journal by identifier, not by unit:

```fish
journalctl --user -t whisrsd -f
```

`journalctl --user -u whisrs` will usually **not** show it. Hook output comes from
the short-lived `sh` child, and journald resolves the owning unit from the sender's
PID — by the time it looks, a command like `echo hi` has already exited, so the
line lands in the journal without the unit attached and the `-u` filter drops it.

### Generate text with no selection

`whisrs command` **rewrites a selection**: highlight some text, press the key,
say what to do with it ("make this formal", "translate to German"). It reads
the primary selection, falling back to a simulated Ctrl+C, and refuses with
"no text selected" when it comes up empty.

To **write new text** where there is nothing to select, use an `[[llm_commands]]`
entry with a generic instruction. That path never reads the selection or the
clipboard, so nothing needs to be highlighted first: press the key, say what you
want, and the result is typed at the cursor.

```toml
[[llm_commands]]
name = "ask"
hotkey = "Super+Shift+B"
instruction = "Treat the following text as a request and output only what is asked. Output the requested artifact itself, with no preamble, no explanation and no code fences."
```

Then press `Super+Shift+B`, say "the command to install steam on arch linux",
press again (or stop talking), and `sudo pacman -S steam` is typed where your
cursor is. Say "a polite email declining the meeting on Thursday" and you get
the email. The instruction is what makes it generic, so one entry covers every
ad-hoc request; a second entry with a narrower instruction ("Translate the
following text into German. Return only the translation.") stays a dedicated
command.

It also works from a compositor keybind:
`bind = $mainMod SHIFT, B, exec, whisrs llm-command ask` on Hyprland,
`bindsym $mod+Shift+b exec whisrs llm-command ask` on Sway.

### What happens to the LLM's reply

The following applies to `whisrs command` and to every `[[llm_commands]]` entry,
since both type the model's reply at the cursor.

- **A wrapping code fence is removed.** If the whole reply is one fenced block,
  the fence goes and the body is typed. A reply containing several fenced blocks
  is prose about code, not a wrapper, and is left as it came.
  There is no opt-out, and that is a real limitation: an `[[llm_commands]]` entry
  whose instruction genuinely asks for a fenced block ("wrap this in a python
  code fence") cannot produce one — the fence is stripped on the way to the
  cursor, and no config key turns that off. If you need the fence characters
  themselves, type them yourself around the result.
- **Multi-line replies are typed normally**, except into a terminal. Translating
  a paragraph or drafting an email is exactly what these commands are for, so
  the line breaks are kept.
- **A multi-line reply is refused when the focused window is a terminal.** There
  a line break is an Enter, which would run a command before you have read it.
  The text goes to the history, so `whisrs log` prints it and you can copy it
  from there. With `[general] history = false` there is no copy, so the result
  is discarded. Ask for a one-liner to get an injected result.
  You are told when this happens: the "not typed" notification fires even with
  `[general] notify = false`, because that setting means "don't narrate normal
  operation", not "discard my dictation quietly".

The refusal also applies with `[input] paste = true`, where the risk is lower
but not gone. Pasting into a terminal sends Ctrl+Shift+V, and a terminal with
bracketed paste enabled inserts multi-line text literally instead of running
each line — but bracketed paste is the foreground program's choice, not the
terminal's, and it is off inside many TUI programs and in some readline modes.
whisrs cannot see which is the case from the window class alone, and refusing
wrongly costs you one `whisrs log` lookup where injecting wrongly runs commands
you never read, so it refuses either way.

The refusal does not apply with `[input] clipboard_only = true`. whisrs then
types and pastes nothing, so a multi-line reply is copied to the clipboard like
any other. If you paste it into a shell yourself, the bracketed paste caveat
above still applies.

Terminal detection needs the compositor to report the focused window class,
which today means Hyprland, Niri, Sway and X11. On KDE and GNOME a terminal is
treated as an ordinary target, so a multi-line reply is typed there. Add any
class the built-in list misses to `[input] terminal_classes`.

Dictation follows the same rule with a gentler outcome. If a transcript or an
`llm_post_process` rewrite contains a line break and the focused window is a
terminal, each run of line breaks becomes a single space, whether the text is
typed or pasted. A `clipboard_only` copy keeps its line breaks. This uses the
same terminal detection, so it does not fire on KDE and GNOME. Control characters such as Escape and Backspace are removed from
everything whisrs types, pastes or copies, wherever it is going.

## Choosing the microphone

`[audio] device` picks the input whisrs records from:

- `"default"` (or empty): the system default input, through ALSA. With the PipeWire or PulseAudio ALSA plugin installed (the usual setup) this follows the default source set in your sound settings.
- A PulseAudio or PipeWire source name, such as `alsa_input.usb-Blue_Microphones_Yeti-00.analog-stereo`. List them with `pactl list short sources` (second column), or pick one from `whisrs config` under the audio device item. The source description shown in your sound settings also works. Monitor sources (names ending in `.monitor`) record what your speakers play.
- An ALSA PCM name, such as `pipewire`, `plughw:1,0` or `sysdefault:CARD=PCH` (see `arecord -L`). This also covers systems without a PulseAudio or PipeWire server.

PulseAudio/PipeWire sources are checked first, then ALSA names. A name that matches nothing logs a warning listing the valid names and falls back to the system default. The daemon reads the device when it starts, so restart it after a change (`whisrs restart`); its startup log shows which device the name resolved to. Audio cues and read-aloud playback always use the default output device.

## Recording

The recording feature records two tracks at once: you, through your
microphone, and what your computer plays, through the system audio. When you
stop, both tracks are transcribed and merged into one timed transcript, saved
as Markdown next to a JSON file that holds the same lines with their
timestamps.

Uses that work today:

- meetings and calls
- lectures, videos and podcasts
- language practice against a video
- interviews

`whisrs recording start [--title ...]` (or the `[hotkeys] recording` binding,
or "Start recording (mic + system audio)" in the tray menu) starts a
recording; `whisrs recording stop` ends it and queues the transcript.
`whisrs recording cancel` stops and deletes it. The `recording` hotkey and
`whisrs recording toggle` (for a compositor binding) start a recording when
none is in progress and stop the one that is.

**Two tracks.** Your microphone and the system audio (what your speakers or
headphones play) are recorded as separate files and transcribed separately.
Your track is labeled with `user_name`, and everything from the system track is
labeled "Others" (`--others NAME` on `start` or `toggle` names it otherwise),
unless its voices are told apart (see "Speakers" below).

**Speakers.** whisrs can tell apart the voices on the system track
(`diarization`). Each voice becomes a speaker, numbered in the order they
first speak: the transcript shows them as "Speaker 1", "Speaker 2" and so on,
and `recording.json` lists them under `speakers` with their ids (`S1`, `S2`,
...), how long each one talks (`talk_ms`) and the method used
(`diarization`, `off` when no voice was found). Nothing is guessed: when the
system audio holds a single voice, it keeps the others label ("Others", or
the name given with `--others`). A voice with less than 5 seconds of talk
and under 2 % of its track is marked `minor` in `recording.json`, and its
lines stay in the transcript; it does not count as a second voice, so a
single main voice keeps the others label beside minor ones shown as
"Speaker N". Speech the voices could not be matched to stays under the
others label. The transcript's front matter lists the speakers that have a
line in it; `recording.json` lists every speaker, so `whisrs recording
name` still finds one that said nothing.

`diarization = "backend"` takes the speakers the transcription backend labels:
Deepgram (`diarize_model=latest`, sent only with the recording's own
requests, never with dictation) or an `asr-sidecar` that returns
`segments[].speaker`. `diarization = "sidecar"` asks a diarization sidecar at
`diarization_url` (see below) while any backend transcribes the words.
`"auto"` (the default) uses the sidecar when `diarization_url` is set, else
Deepgram's speakers with Deepgram, and keeps one speaker per track with every
other backend. `"off"` always keeps one speaker per track. Deepgram includes speaker separation in its
pre-recorded price, but not with its Whisper models: with one of those as
`[deepgram] model`, each track stays one speaker. A backend labels voices
per request, so a track sent as several chunks has its voices told apart per
chunk, and the same person in two chunks becomes two speakers. If Deepgram
refuses speaker separation (an HTTP 400 about diarization, for a project
that cannot have it), the track is sent again without it: the transcript is
written with one speaker for that track, and its notes say why. With Deepgram
a chunk holds an hour of speech: over an hour of speech on one track,
speakers are separated per hour. For `asr-sidecar` a chunk is 10 minutes.

In an in-person meeting where several people share your mic, set
`shared_mic = true`, or pass `--shared-mic` to `start`, `toggle` or `retry`
(`--shared-mic=false` turns it off for one recording). The mic's voices are
then told apart too (`M1`, `M2`, ...), and the mic is you only when it holds
a single voice. With several, mic speech no voice was matched to is shown
as "Mic". A recording keeps the `shared_mic` it started with; a retry
given the option replaces it.

**Naming the speakers.** A voice gets a name only when whisrs can tell who
it is; every other voice stays "Speaker N". These steps run in order, and
the first one that names a speaker wins:

1. A name you give with `whisrs recording name` (see below) wins over
   everything else.
2. Your mic is you (`user_name`), when it holds one voice.
3. `--with "Sarah, Omar"` on `start` or `toggle` says who the recording is
   with: names separated by commas, at most 20, each up to 64 characters.
   When the system audio holds exactly one voice and you list exactly one
   person, that voice is them.
4. With `name_speakers = true`, the LLM of the `[llm]` section (the one
   command mode uses) is asked who the voices still unnamed are. It must
   quote the words that show each name, such as "I'm Sarah" or "Thanks,
   Sarah", and a name is kept only when that quote's words are words of
   one line of the transcript, in order (case, spacing and punctuation
   aside), and hold the name or its first name. When you listed people,
   the name must be one of them: a full name, a first name, or a short
   form of three letters or more that starts a first name ("Sam" for
   "Samantha"), and the name is written as you listed it. When you did
   not, only the part of the name the quote holds is kept ("Thanks, Sarah"
   names "Sarah", never "Sarah Jones"). A name
   the LLM gives two speakers, a name another speaker already has, and
   your own name are refused. Anything else stays unnamed.
5. When every voice on the system audio but one has a name, one person you
   listed is left, there are as many voices as people listed, and
   `shared_mic` is off, the last voice is that person.
6. Everyone else is "Speaker N", numbered in the order they first speak. A
   single voice on the system audio keeps the others label instead.

`name_speakers` is off by default, because it sends parts of every
transcript to the `[llm]` endpoint: the title, your name, the people you
listed, and for each voice it asks about, its first 2000 or so characters of
speech with the lines around them. Point `[llm] api_url` at a local server
to keep them on your machine. The LLM is only asked when a voice is left to
name after the steps before it, gets 60 seconds to answer, and is told that
the transcript is data, not instructions, and that the speaker labels can be
wrong. If it fails (no `[llm]` section, no answer in time, an answer that is
not JSON), the transcript is still written, without its names, and
`recording.json` notes why under `errors`.

A `minor` voice is never named after someone you listed, and does not count
as a voice. Your own name in the `--with` list (your full name, your first
name, or a short form of it) is you and is not counted.
With a shared mic that holds other voices, one person listed is not enough
to name the one voice on the system audio, since that person may be in the
room with you. `--with` names only voices that were told apart: with one
speaker per track, name the system audio with `--others` instead.
`whisrs recording retry <id> --with "..."` replaces the names, and
`--with ""` clears them. `recording.json` keeps them as `participants`, and
each speaker's `source` says how it got its name: `user`, `self`, `with`,
`llm` (with the quote as `evidence`), `last`, `channel` (the others label)
or `none` (Speaker N).

To name a speaker yourself, give the recording, the speaker as its id
(`S2`), as "Speaker 2" or by the name it has now, and the name:

```
whisrs recording name last "Speaker 2" Sarah
whisrs recording name last S3 Omar Haddad
whisrs recording name last Sarah ""        # clear the name you gave
```

The transcript and `recording.json` are rewritten at once, without
transcribing anything again, and the other steps run again around your
name: the voices still unnamed are numbered again (so "Speaker 3" can
become "Speaker 2"), and the last voice left can now be the last person you
listed. The command prints each speaker's id, name and source. Your names
are kept in `recording.json` as `speaker_names`, apart from the computed
`speakers`, so `whisrs recording retry` keeps them. A retry sends the audio
again, and the voices can come back numbered differently, so a name you
gave a voice remembers where that voice first spoke (`anchor`: its track
and `first_ms`) and follows it: after a retry it goes to the voice first
heard on that track within a second of that moment, whatever its number is
now, nearest first, each name and each voice used once. When the track
has voices but none of them is, the name is dropped rather than put on
someone else: the transcript's notes ask you to name the speakers again,
and the notification that the transcript is ready says how many names
were dropped. When a retry tells no voices apart on that track (the
diarization sidecar was down, Deepgram refused), the name is kept as it is
and comes back with the next retry that does. A name
you gave a track's own speaker (you, or the system audio when its voices
are not told apart) stays with it. Clearing a name you gave over the LLM's
brings the LLM's name back only with the next retry, which asks it again.
A recording that is queued or being transcribed is refused until its
transcript is ready.

**A diarization sidecar.** A local server that only finds who speaks when,
so speakers can be told apart offline or with a backend that cannot do it
(Groq, OpenAI, `local-whisper`). whisrs sends it each separated track once,
as the speech only (the silences cut out, as for transcription), and lines up
the transcript's words with the turns it returns: each word takes the turn it
overlaps most, else the nearest one within a second. A line whose words fall
in two turns is split between them; a line without word timings
(`local-whisper`) takes the turn it overlaps most. The contract:

```
POST {diarization_url}   multipart form: file = the track's speech as a 16 kHz mono WAV,
                         num_speakers = N (only when N > 0)
200 {"segments": [{"start": 1.20, "end": 4.85, "speaker": "SPEAKER_00"}]}
```

`start` and `end` are seconds into the WAV, and `speaker` is a string or an
integer that only has to be consistent within one request. `num_speakers`
(or `--num-speakers N` on `start`, `toggle` and `retry`, 0 to 20) tells the
sidecar how many voices the system audio holds, or your mic when the
recording has no system audio; 0, the default, lets the sidecar decide. A
recording keeps the number it started with, and a retry given the option
replaces it.

Two ready sidecars are in
[`contrib/diarization-sidecars/`](../contrib/diarization-sidecars/),
measured on a laptop with 15-minute meetings. `nemotron`, the recommended
one, runs NVIDIA's Nemotron 3 Diarization in `nemo-speech serve`, a single
download with no Python: it takes under a minute for 15 minutes of audio on
the CPU (17 seconds on an Intel GPU), found 4, 4 and 4 voices in the three
four-person meetings, 1 in a video with one teacher and 2 in a two-person
call (beside a 3-second third), and almost never mixes two people up, but
leaves more quiet or short speech without a voice. It ignores
`num_speakers`. `sherpa-onnx` is a small Python server started with `uv`: it
took under 3 minutes for the same audio and gave the most speech the right
voice. Deepgram's own speaker separation (`diarization = "backend"`) needs
no sidecar and comes free with its transcription, but scored clearly worse
than both on the same meetings.

whisrs waits for the sidecar's answer for twice the length of the speech it
sent, and at least 10 minutes. A sidecar that cannot be reached is tried
again like a transcript chunk (5, 15, 30 and 60 seconds apart). The
sidecar's own error (HTTP 500), a crash on the request, no answer in time and
an answer that does not follow the contract are not retried. Whatever
fails, the recording does not: its transcript is written with one speaker
per track, as with `"off"`, `recording.json` notes why under `errors` (the
transcript's notes show it too), and the notification that the transcript
is ready says it has no speakers. The voices the sidecar found are kept in
the recording's `.cache/` folder after the transcript is written, so a retry
with the same sidecar, number of voices and audio reuses them instead of
asking again; they are deleted with the audio.

**Languages.** Each track is transcribed in its own language. `language`
defaults to `[general] language`, and `mic_language` and `system_language`
default to `language`, so dictation and recordings can use different
languages. One recording can override them with `-l` (both tracks),
`--mic-language` and `--system-language`, which win over `-l` for their
track. For language practice against a video, where you repeat lines in
German and the teacher mixes German and English:

```
whisrs recording start -t "Lesson 50" --mic-language de --system-language auto --others Video
```

and for a meeting in another language, `whisrs recording start -l de`. The
languages a recording starts with are saved in its `recording.json` and stay
with it: a later config change does not reach it. To redo a recording in
another language, run `whisrs recording retry <id> -l <language>` (or
`--mic-language`/`--system-language` to change one track); a language you
give replaces the saved one, and a track you leave out keeps its own. The
saved languages are those of the latest attempt: a retry with `-l` that fails
leaves the new languages saved, with the recording marked failed. The
transcript's front matter shows `language` (or `mic_language` and
`system_language` when they differ) and the `model` it was made with.

**Headphones are recommended.** With speakers, the system audio leaks into
your microphone. whisrs filters that echo out of your track before transcribing
it, using the system audio as the reference. Echoed lines that still get
through are dropped, but headphones give a cleaner transcript and avoid the
occasional miss. Echo is also harder to catch when the two tracks are
transcribed in different languages, since a line heard on both no longer
reads the same.

**Dictation is off while a recording is in progress.** The microphone is
shared with the recording, so the transcript would record a dictation as your
speech, and in a call the other people would hear it. Until the recording
stops, `whisrs toggle`, command mode and `[[llm_commands]]` are refused with a
notification saying why. A dictation already running when the recording
started finishes normally, and read-aloud keeps working. The tray icon turns
into a red outlined bubble with a dot while a recording is in progress, and an
amber outlined bubble with three dots while it is transcribed (as long as you
are not dictating at that moment). A recording shows no on-screen overlay and
fires no `[hooks]`, so media that is playing is not paused.

**Backends.** A recording is transcribed from its audio files after it ends,
so it needs a backend that can transcribe a recorded file: groq, openai,
deepgram, asr-sidecar or local-whisper.
`backend` defaults to `[general] backend`. With `deepgram-streaming` the
recording goes to Deepgram's REST API. Groq, Deepgram and OpenAI's `whisper-1`
receive the audio as FLAC, which is lossless and, for speech, usually less than half the size of WAV.
Only the stretches where someone speaks are sent, a second of silence apart, and
the timestamps are mapped back onto the recording. The
realtime backends (`openai-realtime`, `openai-compatible-realtime`) cannot
transcribe a recording: setting one as
`[recording] backend` is a config error. Inheriting one from `[general]` gives a
startup warning when `[recording]` or `[hotkeys] recording` is set, since
recordings would capture audio but fail to transcribe.

With `openai`, recordings use `[openai] model`, except that the realtime-only
models (`gpt-live-transcribe`, `gpt-realtime-whisper`) become `gpt-transcribe`,
since OpenAI's file endpoint refuses them. So an `openai-realtime` dictation
setup can keep its model and still set `[recording] backend = "openai"`.
`[openai] languages` does not apply to recordings; they use their own languages
(see "Languages" above).
Recordings send `[general] vocabulary` as the hint, but not `[general] prompt`,
which is an instruction for dictation. A `local-whisper` recording takes
neither: it decodes without any prompt, so no decoded text is carried
forward within a stretch of speech, which can make whisper repeat itself.

With `local-whisper`, a recording is always decoded in short phrases (at most
about 28 seconds each) whatever `[local-whisper] segmentation` says, `"none"`
included, so long recordings do not fall into whisper's repetition loop.
`flash_attn` applies to recordings as it does to dictation.

**Devices.** `mic_device` defaults to `[audio] device`. `system_device`
defaults to the monitor of your default output device, and the recording
follows it: when the default output changes while it runs (headphones plugged
in or unplugged, another output picked), the system track moves to the new
one within about 2 seconds, and the moment in between is filled with silence
so the track stays in step with your mic. Set it to `"none"` to record only
your mic, or to a specific monitor source from `pactl list short sources`
(names ending in `.monitor`), which is never followed. `whisrs config` lists
the monitors under "Recording". If the system audio cannot be opened, the
recording takes your mic only and notes that in the transcript. With
`mic_device` and `[audio] device` at `"default"`, the mic records the ALSA
default device, which whisrs does not move. Through PipeWire's ALSA plugin
(`pipewire-alsa`, the usual setup) that is PipeWire's default input, and
PipeWire moves it to a new default (a headset plugged in, a USB mic
unplugged) by itself. Through the PulseAudio ALSA plugin, whether it moves
depends on your PulseAudio setup, and on plain ALSA (no sound server) it
stays on the device it opened.

**Where recordings are saved.** Each recording gets its own folder under `dir`
(default `~/.local/share/whisrs/recordings`), named after its start time and
title, for example `2026-10-02_0930_standup/`:

```
2026-10-02_0930_standup/
├── recording.json               # status, tracks, speakers and the timed transcript
├── 2026-10-02_0930_standup.md   # the readable transcript
└── audio/
    ├── mic.wav                  # your microphone
    └── system.wav               # the system audio
```

Folders are created private (0700) and `recording.json` is 0600; an existing
`dir` keeps its own permissions.

`whisrs recording list` shows them, `whisrs recording show last` prints the
latest transcript and `whisrs recording copy last` copies it.

**Summaries and titles.** With `summary = true`, each transcript gets a
summary from the LLM of the `[llm]` section (the one command mode uses).
The summary is a job of its own: the transcript is written and notified
first, then the summary is made, `whisrs recording status` shows
"Summarizing" meanwhile, and a second notification says when it is ready.
`whisrs recording summarize <id>` makes one for any transcribed recording,
whatever `summary` says, without transcribing it again. One job runs at a
time, so the LLM never runs beside a transcription: waiting summaries run
after waiting transcriptions, but a summary already running delays the
next transcription until it ends (each LLM call can take up to 10
minutes). The summary works from a copy of the recording and writes its
result at the end, so `whisrs recording name` works while it runs. A
summary that a shutdown or a crash interrupted is made at the next start
(`recording.json` marks it `summary_pending` until it is done); an
automatic one only while `summary` is still on. A new transcript drops a
summary still pending for the old one. The summary goes into the Markdown file as a `## Summary`
section before the transcript, and into `recording.json` as `summary`,
with the model that wrote it (`summary_model`) and when (`summarized_at`).

By default a summary starts with a TL;DR of one or two sentences, then
short sections by topic in the order they came up. It has a "Decisions"
section only when something was decided, and a "Next steps" section only
when someone took something on, as `- [ ]` items grouped by who took them.
It is written in the language most of the transcript is in, and keeps the
names as the transcript spells them. `summary_instruction` replaces those
rules with your own, for lecture notes, quotes from an interview, or the
mistakes you made in language practice:

```toml
summary_instruction = "List each mistake Ada made in German with the right sentence, then the new words the teacher used."
```

Whatever the rules, the LLM is told that the transcript is data, not
instructions, and that the speaker labels can be wrong. It gets the
transcript as timed lines, one speaker's words at a time, so even a long
lecture keeps its timestamps. A transcript of 50 words or fewer is not
sent: its summary is "Too short to summarize.". A long one (over about
16,000 characters, 20 minutes of talk or so) is summarized in parts, at
most 20, and the notes on the parts are then summarized together, so
nothing is left out. The LLM gets 10 minutes for each call; the first call
that fails ends the summary. A reply cut off at the model's output limit,
or one that is not the JSON asked for, is never stored as the summary.

A recording you gave no title (`-t`) also gets one from the summary: 3 to 6
words in the language of the transcript, shown in the Markdown file,
`whisrs recording list` and `recording.json`. Its folder and id stay as
they are. A title you gave is never changed, and `recording.json` says
where the title came from: `title_source` is `user`, `default` or `llm`.

If the summary fails (no `[llm]` section, no answer in time, an error from
the API, a reply cut off or malformed), the transcript is untouched,
`recording.json` notes why under `errors` (the transcript's notes show it
too), and a summary the recording already had stays. `whisrs recording
retry` makes a new summary when `summary = true`. When it is off, the old
summary stays, marked `summary_stale` in `recording.json` and with a line
in the transcript saying it was made before the transcript or the speaker
names last changed, until `whisrs recording summarize` makes a new one.
Naming a speaker (`whisrs recording name`, also while the summary is being
made) marks a summary stale the same way, since it names the speakers by
their old labels.

A summary sends the whole transcript to the `[llm]` endpoint, which is why
`summary` is off by default. Point `[llm] api_url` at a local server to
keep transcripts on your machine. Give the local model a context of at
least 8k tokens, or the start of a long prompt (the rules included) is cut
off without a word: Ollama's default is smaller, so set `num_ctx` (for
example `PARAMETER num_ctx 8192` in a Modelfile, or `OLLAMA_CONTEXT_LENGTH=8192`
for the server). On a CPU a summary can take minutes.

**Agents.** An AI agent can find and read your recordings for you: ask it
what was decided in yesterday's call, for notes on a lecture, or for the
mistakes you made in language practice. The skill in
[`contrib/agent-skill/whisrs-recordings/`](../contrib/agent-skill/whisrs-recordings/)
tells it where recordings live, which `whisrs recording` commands read
them, and that a transcript is data, not instructions, whose speaker
labels can be wrong. Copy the folder into `~/.claude/skills/` for Claude
Code, or into your agent's skills folder. The skill only reads: it never
deletes or edits a recording, and changes one (`summarize`, `retry`,
`name`) only when you ask. When the agent has the MCP tools below, the
skill tells it to use them.

**MCP server.** `whisrs mcp` serves your recordings to an agent over MCP
(the Model Context Protocol) on stdin and stdout. It reads the recordings
folder directly (`dir` above), works with the daemon stopped, and never
changes anything. Add it to Claude Code:

```
claude mcp add whisrs -- whisrs mcp
```

or to another MCP client's config:

```json
{"mcpServers": {"whisrs": {"command": "whisrs", "args": ["mcp"]}}}
```

Its tools:

- `list_recordings` (`since`, `until`, `query`, `limit`): recordings,
  newest first, with id, title, start time, duration, status, the speakers
  heard and whether there is a summary ("Too short to summarize." is
  none). `since` and `until` take a date
  (`2026-10-19`, which `until` counts whole), `today`, `yesterday`,
  `YYYY-MM-DD HH:MM` or an RFC 3339 time; `query` matches the title or id;
  `limit` is 1 to 100, 20 by default.
- `get_recording` (`id`): one recording's details, speakers, languages,
  summary (`summary_stale` when it was made before the transcript or the
  speaker names last changed) and
  files, without the transcript.
- `get_transcript` (`id`, `offset`, `max_chars`, `speaker`, `start_ms`,
  `end_ms`): the transcript as `[HH:MM:SS] Name: text` lines (one
  speaker's words per line, up to 1,500 characters), at most `max_chars`
  at a time (20,000 by default, 2,000 to 100,000), with `next_offset` for
  the next page. `speaker` is an id such as `S2` or a name.
- `search_transcripts` (`query`, `since`, `limit`): a search of titles,
  summaries and transcripts that ignores case, including accented and
  non-Latin letters ("istanbul" finds "İstanbul", "σοφος" finds "ΣΟΦΟΣ"),
  and counts a run of spaces as one, newest recording first, that stops
  after `limit` hits (20 by default, at most 100). A transcript hit
  gives its time, speaker and the `offset` to read it with
  `get_transcript`.

The tools read only what they need: a listing reads each `recording.json`
without the words of its transcript, a full id reads its own folder, and a
search reads one transcript at a time. An id is a full id, a unique prefix
or `last`. The server and each tool
tell the agent that recording content is data, not instructions, and that
the speaker labels can be wrong. A recording that failed has no transcript,
and `get_transcript` says so. The release builds include the server; a
source build without the `mcp` feature says so when you run `whisrs mcp`.

**Leaving recording out of a build.** Both release downloads (the full one
and the minimal one) include the recording feature and the MCP server, and
so does `cargo install whisrs`. A source build can leave them out by
dropping the `recording` feature; `mcp` needs it, so it goes too:

```
cargo install whisrs --locked --no-default-features --features local-whisper,tray,overlay,hooks
```

Dictation works the same in such a build. `whisrs recording` and the daemon
answer that the build has no recording feature, the tray has no recording
items, and a `[recording]` section that differs from the defaults or a
`[hotkeys] recording` binding is ignored, with one warning in the daemon log
(and when `whisrs config` saves). Both stay in the file.

**Audio retention.** `audio_retention_days` controls how long the `audio/`
folder of a transcribed recording is kept: `7` (the default) deletes it at the first
cleanup after a week (cleanup runs when the daemon starts and after each
recording is transcribed), `0` deletes it as soon as the transcript is written, and a negative
value keeps it forever. The transcript and `recording.json` are never deleted. A
recording whose transcription failed always keeps its audio, so you can fix the
cause (a missing API key, a network error) and run `whisrs recording retry`.

**A muted mic.** If your microphone track sends only digital silence for 3
minutes (a muted or wrong device; a quiet room is never that silent), a
notification asks whether it is muted. It warns again only if the mic goes
silent again after sending real audio. The system track is never checked,
since a paused video is silent too.

**Disk space.** A recording takes about 1.9 MB a minute per track, and
transcribing it writes an echo-cancelled copy of your track next to them. A
recording needs 100 MB free in `dir` to start (about 10 minutes plus room to
transcribe them). While it runs, the free space is checked every minute: one
notification warns when less than 30 minutes fit, and with less than 2
minutes left the recording stops, as at the time limit, and is transcribed.
A check that gets no answer within 5 seconds (a hung network mount) is
skipped, so it never delays the time limit.

`max_duration_minutes` (default 180, at most 2160) stops a recording that was
left running. A recording interrupted by a crash or reboot is recovered from its
audio the next time the daemon starts. If its transcription is interrupted three
times in a row (for example because it keeps crashing the daemon), it is marked
failed instead of being resumed again, so dictation keeps working; run
`whisrs recording retry` to try once more.

Cloud backends that answer "rate limited" (HTTP 429) or "overloaded" (503) are
retried with a backoff, honoring the server's `Retry-After`, for up to five
minutes per chunk. A server error (500, 502, 504, or Cloudflare's 520 to 524,
such as a 524 timeout on a long upload) is retried three times, 5, 15 and 30
seconds apart, except a 500 from `asr-sidecar`, which is usually an error in
the sidecar itself. Each finished chunk is saved in the recording's `.cache/`
folder, so a retry after a failure only sends what was not transcribed yet.
The chunks are deleted once the transcript is written, so a retry of a
finished recording sends both tracks again (a diarization sidecar's voices
stay, see "Speakers").

**No network.** A chunk that gets no answer at all (no network, a name that
does not resolve, a refused or dropped connection, a TLS error) is retried 5,
15, 30 and 60 seconds apart. If the network is still down after that, the
recording is not failed: it waits for the network, shown as `waiting` by
`whisrs recording list` and `whisrs recording status`, with one notification.
The daemon tries it again when it starts and every 10 minutes while it runs,
for up to 24 hours from its first failed try (144 tries at most), however
long ago the recording stopped, and then marks it failed. These tries do not
count towards the three interrupted attempts above. `whisrs recording retry`
tries a waiting recording at once, and a retry that finds no network starts
a new 24 hours. An `asr-sidecar` that cannot be reached (not running, a
network that is down, a request that times out) counts as no network, but
one that takes a chunk and then closes the connection crashed on it, so like
its HTTP 500 that fails the recording without sending the chunk again.
`local-whisper` never waits.

## The vocabulary file

Vocabulary terms can also live in `~/.config/whisrs/vocabulary.txt`, next to
`config.toml`. This exists for setups where `config.toml` is generated and
read-only (Nix, chezmoi, any templated dotfiles), where adding one proper noun
to `[general] vocabulary` would otherwise mean a rebuild. The path is fixed —
there is deliberately no config key naming it, since setting one would need
the very `config.toml` edit the file avoids.

```
# One term per line. Blank lines and lines starting with # are skipped.
whisrs
Claude Code
NixOS
```

A term that starts with `#` is stored with a single leading backslash
(`\#rust`), which is stripped when the file is read; a `#` anywhere else on the
line is literal, so `C# dev` needs nothing.

At daemon startup the file is merged into `[general] vocabulary` — config.toml's
terms first, then the file's, duplicates dropped — before the config is
validated, so the Deepgram keyterm limits and their startup warnings count the
real list. A missing file is simply ignored; the feature is opt-in by creating
it. Like the rest of the config, the file is read once at startup, so run
`whisrs restart` after editing it.

When the file exists, the `whisrs config` vocabulary editor shows the merged
list. If you change that list, the whole of it is written back to
`vocabulary.txt` on save and config.toml is saved with an empty `vocabulary`,
so no term is stored twice. That rewrite is flat, so any comment lines and
blank-line grouping you added by hand are dropped at that point. If you leave
the vocabulary alone — including opening the editor and accepting the list as
shown — saving other settings touches neither store: your `vocabulary.txt`
keeps its comments and config.toml keeps its terms. Only
`vocabulary` gets this treatment; `prompt` and everything else stay in
`config.toml`.

## Word replacements

`vocabulary` only hints the model. When it still mis-hears a term the same way
every time, map the wrong text to the right one:

```toml
[replacements]
"package build" = "PKGBUILD"
"whisper s" = "whisrs"
```

Every dictation is rewritten before it is typed, copied or logged, and so is
every line of a recording's transcript. Keys match
ignoring case, on whole words, with any spaces or hyphens between the words,
so "Package-build" matches too. Quote every key: unquoted,
`node.js = "Node.js"` is a nested table in TOML, so config.toml fails to
parse and the daemon falls back to built-in defaults. Surrounding punctuation is kept
("package build." becomes "PKGBUILD."). Values are inserted exactly as
written. Text is scanned left to right: where several keys start at the same
word the longest one wins, and a replacement is never itself replaced again.
An empty value deletes the match and the spaces after it, but not the
punctuation next to it. Leave filler words to `remove_filler_words`, which
takes their commas too. Command mode and `[[llm_commands]]` instructions are
left alone. With `llm_post_process` on, replacements run first, so the LLM
sees the corrected text (and can still rewrite it).

On streaming backends the text is rewritten one typed chunk at a time. With
`local-whisper` phrase segmentation or `openai-compatible-realtime` a chunk is
a whole phrase, so this only misses if you pause in the middle of a key.
`deepgram-streaming` sends only final results, which end between words: it can
split a multi-word key across two chunks where Deepgram closes a segment
(usually a pause), in which case it does not match, but never cuts a word.
`openai-realtime` streams tokens, so a chunk can also end in the middle of a
word, and a key like "pack" can match the first half of "package". Batch
backends see the whole transcript and are not affected.

## Environment variables

The following variables override the matching `api_key` in `config.toml`:

- `WHISRS_GROQ_API_KEY`
- `WHISRS_DEEPGRAM_API_KEY`
- `WHISRS_OPENAI_API_KEY`
- `WHISRS_ASR_SIDECAR_API_KEY`

With one of these set, the matching `api_key` line can be left out of `config.toml`
entirely. Keep the section itself if you set `model` there.

These provider keys are also used by the matching TTS backend (`groq`/`openai`/`deepgram`) unless `[tts] api_key` is set. The `tts-sidecar` backend needs no key.

`RUST_LOG` controls daemon log verbosity (e.g. `RUST_LOG=debug whisrsd`).

## GNOME overlay

GNOME Wayland does not support the wlroots layer-shell protocol used by Hyprland and Sway. To use `overlay = true` on GNOME, install the bundled GNOME Shell extension — see [`contrib/gnome-shell-extension/README.md`](../contrib/gnome-shell-extension/README.md) for install and update instructions.
