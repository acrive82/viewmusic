#!/usr/bin/env zsh
# Audio-to-photon latency measurement harness (MANUAL procedure).
#
# This script builds ViewMusic with the on-screen performance HUD enabled, then
# prints the manual measurement procedure. The actual latency measurement is a
# MANUAL step (there is no automated audio-to-photon probe in this repo): you
# observe the screen + speaker with a high-frame-rate camera (or the HUD) and read
# the elapsed time off the recording.
#
# Usage:
#   tests/perf/measure.sh
#
# The audio-to-photon latency must be MEASURED on the target hardware (Apple
# Silicon), not estimated. Record results in tests/perf/RESULTS.md.

set -euo pipefail

# Resolve the repo root from this script's location (zsh: ${0:A} = absolute path).
SCRIPT_DIR="${0:A:h}"
REPO_ROOT="${SCRIPT_DIR:h:h}"
RESULTS_FILE="${SCRIPT_DIR}/RESULTS.md"

print -P "%F{cyan}=== ViewMusic perf measurement harness ===%f"
print ""

# ---------------------------------------------------------------------------
# AUTOMATED part: build with the perf HUD feature.
# ---------------------------------------------------------------------------
print -P "%F{green}[1/2] Building viewmusic with --features perf-hud (release)…%f"
print "      (this is the only automated step)"
print ""

cd "${REPO_ROOT}"
cargo build --release -p viz-app --features perf-hud

BIN="${REPO_ROOT}/target/release/viewmusic"
print ""
print -P "%F{green}Build complete:%f ${BIN}"
print ""

# ---------------------------------------------------------------------------
# MANUAL part: the measurement procedure.
# ---------------------------------------------------------------------------
print -P "%F{yellow}[2/2] MANUAL measurement procedure — read carefully%f"
print ""
print -P "%F{yellow}IMPORTANT:%f Do NOT run the binary from this script. Capture taps require"
print "the app to be launched as a signed .app bundle so macOS attributes the"
print "System Audio Recording permission correctly. Launch the built"
print "bundle (or the binary once permission is granted) yourself, then measure."
print ""
print -P "%F{magenta}Method A — 240 fps slow-motion phone camera (preferred, most accurate)%f"
print "  1. Launch ViewMusic and grant System Audio Recording permission if asked."
print "  2. Point a phone camera in 240 fps slo-mo mode at BOTH the speaker grille"
print "     (or a visible woofer) and the ViewMusic window in one frame."
print "  3. Play a track with sharp transients (a metronome / clap works best)."
print "  4. Record ~10 s. In the slo-mo clip, find a transient: note the camera"
print "     frame where the speaker moves (audio onset) and the frame where the"
print "     visualizer reacts (first bar jump / beat flash)."
print "  5. latency_ms = (visual_frame - audio_frame) / 240 * 1000."
print "  6. Repeat for >= 10 transients; record min / median / max."
print ""
print -P "%F{magenta}Method B — audible click + on-screen HUD (quick sanity check)%f"
print "  1. Enable the perf HUD (this build already has it: top-left 'perf' window)."
print "  2. The HUD shows render FPS, frame cost p50/p90 (ms), feature-frame age"
print "     (ms, wall-clock age of the newest audio frame at render time), and the"
print "     degrade state."
print "  3. 'feat age' is the staleness component of audio-to-photon, NOT the full"
print "     path (it excludes capture-buffer + present latency). Use it to confirm"
print "     the pipeline is live and the feature frame is fresh (< ~17 ms); use"
print "     Method A for the authoritative end-to-end number."
print ""
print -P "%F{cyan}Budget:%f hard cap <= 50 ms audio-to-photon; typical ~30 ms."
print ""
print -P "%F{green}Record every run in:%f ${RESULTS_FILE}"
print "  (a table header + row template is already there)."
print ""
print -P "%F{yellow}NOTE:%f steps [2/2] are entirely MANUAL — this script cannot perform the"
print "physical measurement. It only builds the instrumented binary and documents"
print "the procedure."
