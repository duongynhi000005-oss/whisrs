#!/usr/bin/env bash
# whisrs gen-issue208-fixture: synthesize the long dictation fixture for issue #208.
#
# Builds a ~4.5 min (273 s) 16 kHz mono s16 WAV of espeak-ng speech at 130 wpm:
# 41 different segments with pauses of 0.8 to 4 s between them, written as
# exact digital zeros. Three sentences stop mid-way before a 4 s pause.
# scripts/check-issue208.sh uses it to drive local-whisper's
# segmentation = "none" batch path far past 28 s of audio.
#
# espeak-ng 1.52.0 + ffmpeg 9.0.1 reproduce the original fixture byte for byte.
# Other ffmpeg versions change only the encoder tag in the WAV header.
#
# Output (gitignored, never commit the WAV):
#   fixtures/issue208_long.wav   16 kHz mono s16 test audio (~273 s, pauses)
#   fixtures/issue208_long.txt   ground-truth transcript (one line, 433 words)
#
# Requires: espeak-ng, ffmpeg, ffprobe
#
# Usage:
#   ./scripts/gen-issue208-fixture.sh          # writes the fixture
#   ./scripts/gen-issue208-fixture.sh --force  # regenerate even if present

set -euo pipefail

GREEN='\033[32m'
RED='\033[31m'
BOLD='\033[1m'
RESET='\033[0m'

info()  { echo -e "  ${GREEN}${BOLD}$1${RESET} $2"; }
error() { echo -e "  ${RED}$1${RESET}"; }

FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        -h|--help)
            sed -n '2,21p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            error "Unknown flag: $arg"
            exit 1
            ;;
    esac
done

for tool in espeak-ng ffmpeg ffprobe; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        error "Missing required tool: $tool"
        exit 1
    fi
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE_DIR="$REPO_ROOT/fixtures"
OUT_WAV="$FIXTURE_DIR/issue208_long.wav"
OUT_TXT="$FIXTURE_DIR/issue208_long.txt"

if [ -f "$OUT_WAV" ] && [ -f "$OUT_TXT" ] && [ "$FORCE" -eq 0 ]; then
    info "Exists:" "$OUT_WAV (use --force to regenerate)"
    exit 0
fi

mkdir -p "$FIXTURE_DIR"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

# Spoken segments and the silence gap (seconds) inserted AFTER each one.
# Segments 5, 18 and 37 stop mid-sentence before a 4 s pause; the next
# segment finishes the sentence.
SEGMENTS=(
    "This morning I want to talk through the plan for the new release."
    "First, the configuration editor needs a better way to handle lists."
    "Second, we should look at how the daemon starts on older systems."
    "My neighbor bought a red bicycle last spring and rides it every weekend."
    "The library closes at six on Fridays, so plan your visit early."
    "I think the main problem with the"
    "current approach is that it hides errors from the user."
    "Coffee tastes better when the beans are ground just before brewing."
    "Our team met on Tuesday to discuss the budget for next quarter."
    "Please remember to water the plants on the balcony while I am away."
    "Mountains in the north are covered with snow for most of the year."
    "She finished reading the novel in a single afternoon."
    "We need to measure how long each request takes under heavy load."
    "The museum opened a new exhibit about ancient sailing ships."
    "If the train is late again, I will take the bus instead."
    "Grandfather used to tell stories about growing up near the river."
    "A good test should fail loudly when the behavior changes."
    "The bakery on the corner sells fresh bread every morning at seven."
    "Let me check whether the"
    "printer in the office still has enough paper for the meeting."
    "Children were playing football in the park until it got dark."
    "The weather forecast says heavy rain will arrive on Thursday evening."
    "I prefer writing notes by hand because it helps me remember things."
    "Every winter the lake freezes and people go skating on it."
    "Our cat sleeps on the windowsill whenever the sun comes out."
    "The conference keynote focused on privacy and open hardware."
    "Remember to back up your files before upgrading the operating system."
    "He planted tomatoes, peppers, and cucumbers in the small garden."
    "The orchestra rehearsed for three hours before the concert."
    "Traffic on the highway was unusually light this afternoon."
    "Sometimes the simplest solution turns out to be the most reliable."
    "My sister is learning to play the violin at a local school."
    "The hotel room had a beautiful view of the harbor."
    "We should replace the old cables before they start causing trouble."
    "Fresh oranges arrived at the market from the southern farms."
    "After dinner we walked along the beach and watched the sunset."
    "The documentation explains each option with a short example."
    "Before we finish, I want to"
    "thank everyone who helped test the latest build."
    "Tomorrow I will write the summary and send it to the whole group."
    "That is everything for today, thank you for listening."
)
PAUSES=(2 1.5 3 2.5 1 4 2 1.2 3 2 1.5 2.5 1 3.5 2 1.2 2 3 4 1.5 2 1 2.5 3 1.5 2 1 2 3.5 1.2 2 1.5 3 2 1 2.5 1.5 4 2 1.5 0.8)

if [ "${#SEGMENTS[@]}" -ne "${#PAUSES[@]}" ]; then
    error "SEGMENTS (${#SEGMENTS[@]}) and PAUSES (${#PAUSES[@]}) differ in length"
    exit 1
fi

ESPEAK_VOICE="en-us"
ESPEAK_SPEED=130

silence() { # silence <seconds> <outfile>
    ffmpeg -loglevel error -y \
        -f lavfi -i anullsrc=channel_layout=mono:sample_rate=16000 \
        -t "$1" -c:a pcm_s16le "$2"
}

CONCAT_LIST="$WORK_DIR/concat.txt"
: > "$CONCAT_LIST"

# Short lead-in silence so speech doesn't start at sample zero.
silence 0.3 "$WORK_DIR/lead.wav"
echo "file '$WORK_DIR/lead.wav'" >> "$CONCAT_LIST"

for i in "${!SEGMENTS[@]}"; do
    raw="$WORK_DIR/raw_$i.wav"
    seg="$WORK_DIR/seg_$i.wav"
    gap="$WORK_DIR/gap_$i.wav"

    espeak-ng -v "$ESPEAK_VOICE" -s "$ESPEAK_SPEED" -w "$raw" "${SEGMENTS[$i]}"
    # Resample espeak-ng output (22.05 kHz) to the pipeline's 16 kHz mono s16.
    ffmpeg -loglevel error -y -i "$raw" -ar 16000 -ac 1 -c:a pcm_s16le "$seg"
    echo "file '$seg'" >> "$CONCAT_LIST"

    silence "${PAUSES[$i]}" "$gap"
    echo "file '$gap'" >> "$CONCAT_LIST"
done

ffmpeg -loglevel error -y -f concat -safe 0 -i "$CONCAT_LIST" -c copy "$OUT_WAV"

# Ground truth: the segments joined with single spaces, one line.
printf '%s\n' "$(IFS=' '; echo "${SEGMENTS[*]}")" > "$OUT_TXT"

DURATION="$(ffprobe -loglevel error -show_entries format=duration \
    -of default=noprint_wrappers=1:nokey=1 "$OUT_WAV")"

info "Wrote:" "$OUT_WAV (${DURATION%.*}s, 16 kHz mono s16)"
info "Wrote:" "$OUT_TXT (ground truth)"
