# ViewMusic — audio-to-photon latency measurements

Measured (not estimated) on the target hardware. See
[`measure.sh`](./measure.sh) for the build step and the MANUAL measurement
procedure.

- **Budget:** hard cap **≤ 50 ms** audio-to-photon; typical **~30 ms**.
- **Target hardware:** Apple Silicon (M1 Pro reference), macOS ≥ 14.4.
- **Method A** = 240 fps slow-motion camera (authoritative end-to-end).
- **Method B** = perf-HUD `feat age` (staleness component only — NOT end-to-end).

All rows are filled in MANUALLY after a measurement session; this file ships with
the header + a template row only.

## Results

| Date | Machine / chip | macOS | App build | Method | Source signal | Samples | min (ms) | median (ms) | max (ms) | Pass ≤50 ms? | Notes |
|------|----------------|-------|-----------|--------|---------------|---------|----------|-------------|----------|--------------|-------|
| _YYYY-MM-DD_ | _e.g. MacBook Pro M1 Pro_ | _14.x_ | _release + perf-hud_ | _A / B_ | _metronome 120 BPM_ | _10_ | _–_ | _–_ | _–_ | _yes / no_ | _camera fps, anomalies, degrade state_ |

## HUD reference snapshot (optional, Method B)

Record the HUD readings during a representative loud passage:

| Date | render FPS | frame p50 (ms) | frame p90 (ms) | feat age (ms) | degrade state |
|------|-----------|----------------|----------------|---------------|---------------|
| _YYYY-MM-DD_ | _~60_ | _–_ | _–_ | _–_ | _full / shed-trails / PAUSED_ |
