#!/usr/bin/env bash
# whisrs check-issue208: scripted check for issue #208.
#
# With [local-whisper] segmentation = "none", the daemon transcribes a whole
# dictation after it stops. One whisper.cpp call over a long dictation let the
# text decoded in one 30 s window carry into the next, and it looped. Past
# 28 s the dictation is now decoded in windows of at most 28 s, cut at pauses.
#
# Generates the fixture (if missing) and transcribes it the way the daemon
# does for "none" (examples/issue208_batch_check.rs), in English, with a real
# user's prompt that made the unfixed code loop 33 times on it. Passes when the
# transcript has no loop (a 2 to 40 word phrase repeated 3 or more times back
# to back), has at most 1.2x the ground truth's words, and covers at least 90%
# of them.
#
# Needs large-v3-turbo: base.en and small.en never looped on this fixture.
# Takes a few minutes on CPU.
#
# Exit code: 0 = pass, 1 = loop, too many words, low coverage or a harness
# error (bad WAV, failed decode), 2 = model not found. A failed build exits
# with cargo's code.
#
# Usage:
#   ./scripts/check-issue208.sh                 # uses ~/.local/share/whisrs/models/ggml-large-v3-turbo-q5_0.bin
#   WHISRS_ISSUE208_MODEL=/path/to/model.bin ./scripts/check-issue208.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WAV="$REPO_ROOT/fixtures/issue208_long.wav"
TRUTH="$REPO_ROOT/fixtures/issue208_long.txt"
MODEL="${WHISRS_ISSUE208_MODEL:-$HOME/.local/share/whisrs/models/ggml-large-v3-turbo-q5_0.bin}"
# Kept verbatim, typo included: this exact prompt made the unfixed code loop.
PROMPT="make sure to spell out words like 'slash', 'question mark', etc, as pancuation marks, like '!', '/', etc, rather than actual text. whisrs, nvim, hyprland, claude code"

if [ ! -f "$WAV" ] || [ ! -f "$TRUTH" ]; then
    "$REPO_ROOT/scripts/gen-issue208-fixture.sh"
fi

if [ ! -f "$MODEL" ]; then
    echo "Model not found: $MODEL" >&2
    echo "Download ggml-large-v3-turbo-q5_0.bin, or set WHISRS_ISSUE208_MODEL." >&2
    exit 2
fi

cd "$REPO_ROOT"
cargo build --release --example issue208_batch_check --features local-whisper
BIN="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/examples/issue208_batch_check"

echo
echo "=== long dictation, segmentation \"none\" (loop + coverage gate) ==="
status=0
WHISRS_ISSUE208_LANG=en \
    WHISRS_ISSUE208_PROMPT="$PROMPT" \
    WHISRS_ISSUE208_MIN_COVERAGE=0.9 \
    "$BIN" "$WAV" "$MODEL" "$TRUTH" \
    || status=$?

echo
if [ "$status" -eq 0 ]; then
    echo "issue #208 check: OK"
else
    echo "issue #208 check: FAIL (exit $status)"
fi
exit "$status"
