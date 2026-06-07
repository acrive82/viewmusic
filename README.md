# ViewMusic

A Winamp-style real-time music visualizer for macOS and Windows. It listens to whatever is
playing on your machine — any app, no loopback drivers, no audio routing — and renders
colorful, beat-reactive visuals at a hard 60 fps with low audio-to-photon latency.

Visualizations ("artifacts") are **plain JSON files**: declarative scenes whose geometry,
motion, and color are mathematical formulas over the live audio features (48-band spectrum,
waveform, loudness, beat events), time, and your own settings. Write your own with nothing
but a text editor — the app picks them up from a folder and reloads on demand.

## Features

- **15 built-in visualizers** — spectrum bars, oscilloscope, radial pulse, color field,
  starfield warp, spectrum galaxy, pixel rain, aurora waves, bass tunnel, breathing grid,
  DNA helix, kaleido petals, liquid spectrum, neon gauges, and particle burst.
- **JSON artifacts** — every visual is a `.artifact.json` file: declarative geometry,
  motion, and color written as small math formulas over the live audio. No build step, no
  plugins, no compiling — drop a file in a folder and reload.
- **Live settings** — each artifact declares its own controls (sliders, toggles, dropdowns,
  color pickers). They auto-render in a top-right panel and apply within a single frame, with
  one-click reset to defaults. Your selection and settings persist across restarts.

## Requirements

- **macOS** 14.4 or newer (the floor for the public system-audio capture permission flow),
  Apple Silicon, or
- **Windows** 10 version 1803 or newer, 64-bit (audio capture uses WASAPI loopback — no
  permission prompt, no driver)
- Rust 1.87 or newer, to build from source

## Build from source (macOS)

```bash
git clone https://github.com/acrive82/viewmusic.git
cd viewmusic
./packaging/bundle.sh                       # release build + .app assembly + codesign
open target/release/bundle/ViewMusic.app
```

`packaging/bundle.sh` produces `target/release/bundle/ViewMusic.app`. If a stable
**Apple Development** signing identity is present in your keychain, the script signs the
bundle with it; otherwise it falls back to an **ad-hoc** signature and prints a warning.

> **Why the signing identity matters:** macOS keys the System Audio Recording permission
> grant to the app's code signature. With an ad-hoc signature the signature changes on every
> rebuild, so the grant does **not** persist — macOS will prompt for permission again after
> each rebuild. Creating a free Apple Development certificate (Xcode → Settings → Accounts,
> or the Apple Developer site) gives you a stable identity so the grant survives rebuilds.

## First launch: audio permission

On first launch ViewMusic asks for the **System Audio Recording** permission so it can read
the audio your Mac is playing. Click **Allow**. If you deny it (or want to change it later),
grant it under **System Settings → Privacy & Security → System Audio Recording**, then
relaunch the app. ViewMusic never records or stores audio — it only reads the live mix to
drive the visuals.

> The permission is tied to the app bundle. Launch ViewMusic via the `.app`
> (`open …/ViewMusic.app`), not by running the bare binary — a terminal-launched binary is
> not recognized as the app and the permission prompt will not appear.

## Build and run (Windows)

Windows capture uses **WASAPI loopback**: it records the system mix directly, with no driver,
no audio routing, and — unlike macOS — **no permission prompt**. Just run the app and play
audio.

From a PowerShell prompt with Rust installed:

```powershell
git clone https://github.com/acrive82/viewmusic.git
cd viewmusic
.\packaging\bundle-windows.ps1              # release build + portable zip
```

`bundle-windows.ps1` builds `viz-app` for the `x86_64-pc-windows-msvc` target with the Visual
C++ runtime linked statically (`-C target-feature=+crt-static`), then packages
`viewmusic.exe` plus a short usage note into `target\viewmusic-windows-x64.zip`. The static
runtime means the exe runs on a clean Windows 10 1803+ machine with **no Visual C++
Redistributable** required. Unzip it anywhere and double-click `viewmusic.exe`.

> **SmartScreen:** this build is **not code-signed**, so Microsoft Defender SmartScreen may
> show an "unrecognized app" warning on first run. Click **More info → Run anyway**. The
> warning fades as the download accrues reputation; it is not a security gate.

## Usage

- **Visualizer dropdown (top-right):** pick any of the 15 built-ins or any artifact you have
  authored.
- **Settings panel (top-right):** auto-generated controls for the selected artifact; changes
  apply instantly, with a one-click reset. Your selection and settings are remembered between
  runs.
- **Reload artifacts:** rescans your artifacts folder so you can iterate without restarting.
- The overlay auto-hides after a few seconds of no mouse movement; move the mouse to bring it
  back.

Just play music from any app — the visualizer reacts immediately.

## Author your own artifact

1. Start with the **[authoring manual](docs/authoring/README.md)** — a step-by-step
   tutorial, a complete reference, copy-paste recipes, and an explained gallery of every
   built-in. The manual is the place to learn; the
   **[artifact contract](docs/reference/artifact-contract.md)** is the normative, last-word
   specification of every field, limit, and function.
2. Drop a `<name>.artifact.json` file into the artifacts folder:
   - macOS: `~/Library/Application Support/io.github.acrive82.viewmusic/artifacts/`
   - Windows: `%APPDATA%\acrive82\viewmusic\config\artifacts\`
3. Click **Reload artifacts** in the overlay, then pick your artifact from the dropdown.
4. If it does not appear, the log explains why — naming the file, the JSON path, and the
   offending token:
   - macOS: `~/Library/Logs/io.github.acrive82.viewmusic/viewmusic.log`
   - Windows: `%LOCALAPPDATA%\acrive82\viewmusic\data\logs\viewmusic.log`

The machine-readable JSON Schema lives at
[`docs/reference/artifact.schema.json`](docs/reference/artifact.schema.json) (regenerate it
with `cargo run -p viz-contract --bin export-schema`).

## Platform support

ViewMusic runs on **macOS and Windows.** Audio capture is the only platform-specific layer,
isolated behind a single capture seam: macOS uses the Core Audio process-tap API, and Windows
10 1803+ (x64) uses WASAPI loopback. Everything downstream — the DSP, beat detection,
renderer (wgpu: Metal on macOS, DX12 on Windows), windowing, UI, and the artifact engine — is
shared, portable Rust. The component-by-component design record for the Windows port is in
[`docs/windows-port-analysis.md`](docs/windows-port-analysis.md).

## Building and testing

```bash
cargo build --workspace
cargo test --workspace      # DSP synthetic-signal tests, formula VM property tests,
                            # the hostile-artifact suite, determinism, and a GPU smoke test
cargo clippy --workspace --all-targets
```

The test suite is hardware-free and runs identically on both platforms. CI
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) builds and tests on macOS (Apple
Silicon) and Windows x64 on every push and pull request; the Windows job runs the GPU smoke
test against the WARP software adapter and uploads the portable zip. The Windows backend can
be cross-checked from any host with `cargo check --target x86_64-pc-windows-msvc --workspace`.

Workspace layout: `viz-core` (shared types) · `viz-expr` (formula bytecode VM) ·
`viz-contract` (JSON contract, loader, artifact library) · `viz-audio` (system-audio capture
— Core Audio tap on macOS, WASAPI loopback on Windows — + DSP) · `viz-render` (wgpu render
pipelines) · `viz-app` (winit/egui shell).

## License

ViewMusic is released under the MIT License. See [`LICENSE`](LICENSE).
