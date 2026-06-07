# Windows Port: Feasibility and Cost Analysis

> **Status: implemented.** The Windows port described below has shipped. ViewMusic
> now runs on Windows 10 1803+ (x64) via a WASAPI loopback capture backend
> (`crates/viz-audio/src/tap_windows.rs`) behind the existing capture seam, with a
> watchdog that treats idle silence as healthy, platform-correct paths and UI, and a
> portable zip bundle (`packaging/bundle-windows.ps1`). Build it yourself by cloning
> the repository and running `packaging\bundle-windows.ps1` (see the README). This
> document is retained as the **design record** — the component-by-component
> analysis, the capture deep-dive, and the risk register that guided the work.

ViewMusic is a real-time system-audio visualizer. It captures whatever audio your
machine is playing, runs a small DSP pipeline over it (spectrum bands, energy,
onset/beat detection), and renders GPU visualizations described by a small JSON
"artifact" format.

This document answers one question for prospective contributors and users: **what
does it take to run ViewMusic on Windows?** Every claim about ViewMusic's structure
is grounded in the source tree; every claim about the Windows platform is grounded in
current (2025/2026) vendor documentation and the state of the relevant Rust
ecosystem.

---

## 1. Executive summary

**Recommendation: feasible and low-risk; defer until there is demonstrated Windows
demand, then execute the phased plan below. Estimated total effort: 15–30 person-days
for a working, signed Windows build by one experienced Rust developer who already
knows this codebase.**

The reason the number is this small is structural. ViewMusic was built with the
platform boundary in the right place. The macOS-specific code is almost entirely
confined to a single module, `crates/viz-audio/src/tap.rs`, behind two traits —
`CaptureSource` and `SampleSink` — that already describe "start a capture that
delivers mono f32 hops to a sink." Everything downstream of that seam (the DSP, the
beat detection, the renderer, the windowing, the UI, the artifact engine) is either
pure Rust or built on cross-platform crates (wgpu, winit, egui) that have first-class
Windows support. The non-macOS build path already exists and already compiles — it
just returns "unsupported" at the capture seam (`crates/viz-audio/src/tap.rs`,
`fallback` module).

So the port is not a rewrite. It is, in order of effort:

1. **One new file**: a WASAPI loopback `CaptureSource` implementation (the real
   work).
2. **A handful of small adaptations**: a Windows log path, packaging, and a
   simplification of the permission state machine (Windows needs far less of it).
3. **CI plumbing**: a Windows job that runs the (already platform-agnostic) test
   suite, plus a decision on whether to run the GPU smoke test on a software adapter.

The dominant risks are not technical blockers — they are *behavioral* differences in
how Windows loopback handles silence and device changes, which interact with
ViewMusic's existing capture-health watchdog. Those are spelled out in §3 and §5.

---

## 2. Per-component portability table

**Stated assumption for all effort ranges:** one experienced Rust developer who
already knows this codebase, working without prior WASAPI experience. Ranges, not
points; the spread reflects genuine unknowns (named in §5), not padding.

Classification legend: **As-is** = compiles and runs on Windows unchanged ·
**Adapt** = small, localized changes · **Replace** = needs a new platform backend.

| # | Functional area | Crate / location | Classification | Effort (person-days) | Rationale (grounded in code) |
|---|---|---|---|---|---|
| 1 | **Audio capture** | `viz-audio/src/tap.rs` (macOS `CidreTap` + cidre) | **Replace** | 6–12 | The only genuinely platform-bound component. macOS uses a Core Audio process tap wrapped in a private aggregate device (`TapDesc::with_stereo_global_tap_excluding_processes`, `AggregateDevice`, `AudioDeviceIOProc`). None of this exists on Windows. A new `CaptureSource` impl over WASAPI loopback is required. The trait seam (`CaptureSource`, `SampleSink`) and the platform-selected `tap::start` constructor already exist, so the new backend slots in without touching callers. See §3. |
| 2 | **Audio analysis (DSP)** | `viz-audio/src/dsp.rs`, `onset.rs` | **As-is** | 0 | Pure Rust over `realfft`. The module docstring states it is "deliberately hardware-free." No OS calls, no platform `cfg`. The synthetic-signal tests already exercise it identically on any host. |
| 3 | **Rendering** | `viz-render/*` (wgpu 29) | **As-is** (1 line) | 0.5–1 | wgpu is cross-platform; on Windows the default backend is DX12. The one Windows-relevant detail: `viz-render` itself contains no backend selection — backend choice lives in the app/window layer and the smoke test (both force `Backends::METAL | PRIMARY`). The renderer's shaders are WGSL and portable. Effort is for verifying blend/feedback paths on DX12, not code changes here. |
| 4 | **Windowing / event loop / GPU context** | `viz-app/src/window.rs` (winit 0.30) | **Adapt** | 1–3 | winit 0.30 fully supports Windows. `GpuContext::new` hardcodes `backends = Backends::METAL | Backends::PRIMARY` (line ~72) — on Windows this must include/default to DX12 (`Backends::DX12` or `PRIMARY`, which already includes it). Frame pacing (`should_present`, hard 60 Hz cap) is pure and portable, but its premise ("macOS throttles `request_redraw` to the display link") differs on Windows — see §3 (variable refresh). |
| 5 | **UI overlay** | `viz-app/src/overlay.rs`, `settings_panel.rs`, `hud.rs` (egui 0.34 + egui-wgpu/egui-winit) | **As-is** | 0–1 | egui/egui-wgpu/egui-winit are platform-agnostic and ride on the same wgpu device and winit window. No platform code in these modules. Budget is for visual QA (font/DPI rendering on Windows), not porting. |
| 6 | **Artifact engine (expr VM + contract)** | `viz-expr/*`, `viz-contract/*`, `viz-core/*` | **As-is** | 0 | All pure Rust. `viz-core` depends only on `serde`; `viz-expr` is a self-contained lexer/parser/VM; `viz-contract` is JSON loading + schema validation (`schemars`, `jsonschema`). Built-in artifacts are `include_str!`-embedded, so no path assumptions. |
| 7 | **Persistence** | `viz-app/src/persist.rs` (`directories` crate) | **As-is** | 0–0.5 | State paths use `directories::ProjectDirs`, which resolves to `%APPDATA%\...` on Windows automatically. No hardcoded macOS paths in this module. Budget is only for confirming round-trip on Windows. |
| 8 | **Logging** | `viz-app/src/logging.rs` (`tracing`, `tracing-appender`) | **Adapt** | 0.5–1 | The logging stack is portable, but the log directory is **hardcoded to the macOS convention**: `log_dir_from_home` builds `~/Library/Logs/<bundle id>` directly (the comment notes `directories` "does not expose a macOS Logs location"). Windows needs an equivalent — e.g. `directories::ProjectDirs::data_local_dir()` / `%LOCALAPPDATA%\<app>\logs`. Small, localized, with an existing unit-test seam (`log_dir_from_home` is pure). |
| 9 | **Packaging / signing** | `packaging/bundle.sh`, `Info.plist` | **Replace** | 3–6 | macOS-specific: a zsh script that assembles a `.app` bundle and runs `codesign` against an Apple Development identity; `Info.plist` declares `NSAudioCaptureUsageDescription` and `LSMinimumSystemVersion`. None of this applies to Windows. A Windows equivalent (portable `.exe`, optional MSIX, optional signing) must be authored from scratch. See §4 and §5. |
| 10 | **Permissions model** | `viz-audio/src/permission.rs`, `watchdog.rs`, capture-failure UX in `viz-app` | **Adapt (simplify)** | 1–3 | macOS requires a TCC "System Audio Recording" grant; ViewMusic has a whole pure state machine (`PermissionTracker`: Unknown → Granted/Denied, sticky-Granted, three-condition revocation) plus a discriminator that reads `kAudioDevicePropertyDeviceIsRunningSomewhere`. **Classic WASAPI loopback on Windows needs no such grant**, so most of this machinery becomes inert on Windows. The state machine is pure and can stay, defaulting to `Granted`; the work is wiring the Windows backend to feed it sensibly. See §3. |
| — | **Test infrastructure** | `viz-render/tests/gpu_smoke.rs`, `tests/perf/`, `tests/contract/` | **Adapt** | 2–4 | Contract/hostile tests and DSP/expr unit tests are pure and run anywhere. The headless GPU smoke test forces `Backends::METAL | PRIMARY` and asserts an adapter exists ("Metal is always present on this Mac, so no env guard"); on a Windows CI runner it must select DX12 and tolerate a software (WARP) adapter or be gated. Perf measurement (`tests/perf/`) is a manual, hardware-specific procedure (slow-motion camera) — a Windows latency figure would have to be measured separately, not ported. |

**Component coverage:** all six workspace crates plus packaging, the permissions
model, and test infrastructure are classified above — 100% of the system.

---

## 3. Capture deep-dive: Core Audio process tap → WASAPI loopback

This is where the port lives or dies, so it gets the most space.

### 3.1 What maps 1:1 — the seam already exists

ViewMusic does not couple the pipeline to Core Audio. In `crates/viz-audio/src/tap.rs`
the contract is two small traits:

- `trait SampleSink: Send` — `fn push(&mut self, mono: &[f32])`, called from inside
  the audio callback, "allocation-free, lock-free, and non-blocking."
- `trait CaptureSource: Send` — `fn granted_buffer_frames(&self) -> u32`; dropping it
  tears the capture down.

The macOS backend, `CidreTap`, is just one implementor. The constructor is selected
by `cfg`: on macOS `pub use macos::start`, on every other platform `pub use
fallback::{start, NoTap}` — and the fallback already exists and already compiles,
returning `CaptureError { stage: Unsupported }`. The pipeline
(`crates/viz-audio/src/pipeline.rs`) calls `crate::tap::start(Box::new(sink))` and
never names a platform; the watchdog owns a `Box<dyn CaptureSource>` and rebuilds it
through the same `tap::start` entry point.

**Consequence:** a Windows port adds a `#[cfg(target_os = "windows")]` backend module
and points `start` at it. The DSP, the lock-free hand-off (triple-buffer frames +
`rtrb` beat queue), the render-thread `AudioHandle`, and the watchdog's rebuild logic
are all reused unchanged. The IOProc's job — mix interleaved stereo f32 to mono and
`push` one callback's worth into the sink — maps directly onto a WASAPI capture loop
that reads packets from `IAudioCaptureClient`, mixes to mono, and calls the same
`push`. The mono-mix arithmetic (`0.5 * (L + R)`) is identical.

### 3.2 The Windows mechanism

Windows offers two routes to "record what you hear":

1. **Classic WASAPI loopback** — open the *render* endpoint and initialize the
   capture stream with `AUDCLNT_STREAMFLAGS_LOOPBACK` (shared mode only; exclusive
   mode cannot be looped back). This captures the full system mix. Per Microsoft's
   documentation it works "regardless of whether the audio hardware contains a
   loopback device, or whether the user has enabled the device," and event-driven
   loopback has been supported since Windows 10 1703. This is the closest analogue to
   ViewMusic's "stereo global tap excluding no processes" and is the recommended
   starting point.

2. **Per-process Application Loopback** — `ActivateAudioInterfaceAsync` with
   `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS` / `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`,
   available since Windows 10 version 2004 (build 19041) and Windows 11. This lets a
   client include or exclude a specific process tree. ViewMusic captures *everything*,
   so per-process filtering is not needed for v1; classic loopback is simpler and has
   a longer support tail.

**Rust paths**, in rough order of "control vs. convenience":

- **`windows` crate (windows-rs)** — direct WASAPI bindings. Maximum control,
  matches how `tap.rs` already talks to the OS at a low level via cidre, and gives a
  clean place to implement `granted_buffer_frames`. Most code to write, but no
  abstraction surprises. Recommended for the core backend.
- **`wasapi` crate** — a safe Rust wrapper over WASAPI (including loopback) on top of
  windows-rs. Could cut the new-file effort noticeably if its abstractions fit the
  allocation-free callback discipline.
- **`cpal`** — cross-platform audio I/O. cpal *does* support WASAPI loopback (it
  detects an output device in `build_input_stream` and sets the loopback flag). It is
  the lowest-effort path and would also unify the audio backend across OSes long term.
  The caution: cpal's stream model and its handling of silence/format negotiation
  must be checked against ViewMusic's real-time rules (no allocation, no blocking in
  the callback) and against the silence behavior below.

### 3.3 What disappears — the permission machinery

On macOS, ViewMusic carries substantial permission infrastructure because there is
**no public status query** for the TCC "System Audio Recording" grant — the only
signal is "attempt, then observe." Hence `permission.rs`'s three-state machine
(`Unknown`/`Denied`/`Granted`, sticky-Granted, a five-second grace window, a
three-condition revocation verdict) and the `output_device_running_somewhere()`
discriminator that reads `kAudioDevicePropertyDeviceIsRunningSomewhere` to tell
genuine silence from a revoked grant.

**On Windows, classic loopback requires no user consent for a full-trust desktop
app.** There is no TCC equivalent, no prompt, no preflight, and no revocation event
to model. This is a net *simplification*:

- The permission state machine can remain (it is pure, well-tested, and the render
  loop already reads it every frame) but on Windows it effectively pins to `Granted`
  once capture starts — the `Unknown → Denied` grace path and the revocation verdict
  never need to fire from a real OS denial.
- The macOS-only guidance string ("Enable system-audio capture in System
  Settings…") must be conditionalized; the Windows failure UX is about *device*
  problems (no render endpoint, exclusive-mode lock), not permissions.
- One honest caveat to verify (§5, Risk 2): if ViewMusic is ever distributed as a
  **packaged** (MSIX/AppContainer) app rather than a plain desktop `.exe`, Windows
  privacy controls can treat audio capture like microphone access and gate it. The
  plain portable `.exe` route avoids this; choosing MSIX reintroduces a (different,
  smaller) permission surface.

### 3.4 What's new — behaviors macOS does not have

Three things the WASAPI backend must handle that the Core Audio backend never did:

1. **Silence delivers no buffers.** This is the most important behavioral
   difference. macOS loopback keeps the IOProc firing during silence (which is why
   ViewMusic's watchdog treats *zero-RMS while running* as a potential fault and the
   permission machine uses the device-running discriminator). **WASAPI loopback does
   not deliver capture buffers while nothing is being rendered** — when playback
   stops, no data arrives, and when it resumes WASAPI reports a discontinuity glitch.
   This directly affects the watchdog in `watchdog.rs`/`pipeline.rs`: its core rule is
   "zero non-zero hops for ~3 s while `running` ⇒ rebuild the capture." On Windows,
   *normal silence* would look exactly like that fault and trigger needless rebuilds.
   The fix is straightforward and localized — the Windows backend must distinguish
   "no buffers because nothing is playing" (healthy idle) from "no buffers because
   capture is broken," e.g. by not advancing the activity clock during legitimate
   idle, or by feeding zero-fill hops to keep the DSP clock moving (the analyzer
   already advances its sample counter through zero-fill: `clock_advances_with_zero_fill`).
   This is the single most important thing to get right and the main reason capture is
   estimated at up to 12 days. (A common industry workaround — playing a continuous
   silent stream so loopback never stalls — is available as a fallback.)

2. **Device-change handling.** On Windows the default render endpoint changes more
   routinely than on macOS (plugging in USB headphones, switching to HDMI, Bluetooth
   connect/disconnect), and the loopback stream is bound to a specific endpoint. The
   backend should subscribe to `IMMNotificationClient` device-change notifications
   and rebuild against the new default endpoint. ViewMusic already has the rebuild
   *mechanism* — the watchdog drops and recreates the `CaptureSource` via
   `tap::start` — so this is about *triggering* a rebuild on a device-change event
   rather than only on the zero-buffer heuristic.

3. **Exclusive-mode applications.** Loopback works only on shared-mode render
   streams. If another app holds the endpoint in exclusive mode (some pro-audio /
   ASIO setups, certain games), loopback cannot capture it. This is a genuine
   limitation with no macOS analogue; the right response is a clear, Windows-specific
   message via the existing `CaptureError` path (a new `CaptureStage` variant fits
   the existing model), not a workaround.

Format handling is also slightly different in spirit: macOS hands the IOProc
interleaved float32 directly; WASAPI hands you the mix format from `GetMixFormat`,
which is typically float32 but must be read and respected. The mono mix and the fact
that the analyzer expects `viz_core::SAMPLE_RATE` mean a resample or a configurable
sample rate may be needed if the device mix rate differs from the analyzer's assumed
48 kHz — a known, bounded piece of work, not a surprise.

---

## 4. Phased plan (de-risk first)

The ordering front-loads the only real unknown (capture) and leaves the
lowest-uncertainty work (packaging) for last.

**Phase 0 — Compile and run the shell on Windows (0.5–1 day).**
Build the workspace on Windows (the non-macOS path already compiles), fix the
backend-selection line in `window.rs` so wgpu picks DX12, and confirm the app opens a
window, renders a built-in artifact against a *silent* pipeline, and the UI works.
This proves rendering + windowing + UI + artifact engine + persistence end-to-end
with **zero** capture code, immediately validating the §2 "As-is" rows.

**Phase 1 — Capture spike (the de-risking core, 4–8 days).**
Write a throwaway WASAPI loopback prototype (windows-rs or `wasapi`) that prints RMS.
Answer the open questions empirically: silence behavior, mix format/sample rate,
device-change events, behavior under exclusive-mode contention. The deliverable of
this phase is *knowledge*, not production code — it converts the §5 risks into facts.

**Phase 2 — Productionize the `CaptureSource` (3–5 days).**
Turn the spike into a real `#[cfg(target_os = "windows")]` backend behind
`CaptureSource`/`SampleSink`, allocation-free in the callback, with full teardown on
`Drop`. Adapt the watchdog's silence interpretation (Phase-1 finding) and the
permission machine (pin to `Granted`; conditional guidance text). Wire device-change
rebuilds through the existing rebuild path.

**Phase 3 — CI matrix (2–4 days).**
Add a Windows job to GitHub Actions running the pure tests (DSP, expr, contract,
permission/watchdog policy, persistence). Decide the GPU smoke test policy: GitHub's
standard Windows runners have no dedicated GPU, so the DX12 smoke test must either run
against the **WARP** software adapter (verify it passes ViewMusic's blend/feedback
assertions — WARP fallback has had documented quirks) or be gated to GPU runners /
skipped on CI. This phase makes Windows a continuously verified target rather than a
one-off build.

**Phase 4 — Packaging and signing (3–6 days, last on purpose).**
Produce a portable release `.exe` (the simplest distributable; the binary is already
`viewmusic`). Optionally add an MSIX package and `winget` manifest for discoverability.
Decide on signing (§5, Risk 3). Author a Windows section of the README/quickstart.
This is left last because it has the least technical uncertainty and depends on a
working binary from Phases 1–3.

---

## 5. Risks and mitigations

**Risk 1 — Loopback silence behavior fights the capture-health watchdog.**
*Likelihood: high. Impact: medium.* WASAPI loopback delivers no buffers during
silence, but ViewMusic's watchdog interprets "zero non-zero hops while running" as a
fault and triggers tap rebuilds with backoff. Left unadapted, the Windows app would
rebuild the capture every few seconds of quiet and log spurious failures.
*Mitigation:* resolve in the Phase-1 spike, then adapt the Windows backend to either
emit zero-fill hops during legitimate idle (the analyzer already advances its clock
through zero-fill and the pipeline tolerates it) or to not mark the watchdog's
`running` activity clock as faulted during a genuine idle endpoint. The watchdog
policy itself is pure and unit-tested, so the change is testable without hardware.

**Risk 2 — Permission/privacy surface depends on the packaging choice.**
*Likelihood: medium. Impact: low–medium.* A plain full-trust desktop `.exe` needs no
audio consent for classic loopback. But if ViewMusic is later packaged as MSIX/UWP
(AppContainer), Windows privacy settings can gate audio capture as if it were
microphone access, reintroducing a (different) permission flow — exactly the kind of
machinery the macOS build has and the Windows build was supposed to shed.
*Mitigation:* ship the portable `.exe` first (Phase 4) to keep the permission surface
at zero; treat MSIX as an explicit, separate decision with its own UX work, and reuse
the existing (pure) permission state machine if it is taken.

**Risk 3 — Code-signing economics and the SmartScreen reputation cliff.**
*Likelihood: high (it will happen). Impact: medium (UX, not engineering).* An
unsigned (or newly-signed) Windows binary trips Microsoft Defender SmartScreen with a
scary "unrecognized app" warning until the signing identity accrues reputation.
Notably, the old shortcut — buying an **EV** certificate for *instant* SmartScreen
reputation — **no longer works (removed in 2024)**: EV-signed files now build
reputation through downloads just like OV. So paying the EV premium (~US$400+/yr,
hardware token) buys little for this use case. Options today: an **OV** certificate
(cheaper, same SmartScreen behavior as EV now), or **Azure Trusted Signing /
Artifact Signing** (~US$10/month, cloud-based, integrates with CI) — but Trusted
Signing onboarding has been **restricted during preview to US/Canada organizations
with 3+ years of history**, which may exclude an individual or new project. There is
**no notarization equivalent** to macOS — signing only suppresses the warning over
time; it is not a gate. *Mitigation:* for an early public release, ship the portable
`.exe` **unsigned** with clear README instructions for the SmartScreen prompt, and
treat signing as a later, optional, low-engineering step once distribution volume
justifies it. Do not let signing block the port.

**Risk 4 (secondary) — Variable-refresh / frame pacing assumptions.**
*Likelihood: medium. Impact: low.* The frame-pacing comment in `window.rs` is written
around macOS display-link throttling and ProMotion (120 Hz). Windows presentation
under DX12 with `PresentMode::Fifo` behaves differently, and high-refresh / VRR
monitors are common. The pure `should_present` 60 Hz cap is portable, but its driving
assumption ("the OS re-requests redraw at vsync") needs validation; a busy
redraw-request loop may behave differently on Windows. *Mitigation:* verify in Phase
0 and adjust pacing if the effective frame rate is wrong; this is QA, not a redesign.

**Risk 5 (secondary) — Headless DX12 on CI runners.**
*Likelihood: medium. Impact: low.* The GPU smoke test assumes an always-present GPU
("Metal is always present on this Mac"). Standard GitHub Windows runners have no real
GPU; WARP (software DX12) exists but has had fallback quirks that could fail the
test's blend/feedback assertions. *Mitigation:* decide in Phase 3 — run the smoke
test against WARP and accept/triage any quirks, gate it behind GPU runners, or skip it
on CI while keeping the pure tests as the Windows gate.

---

## 6. Total effort and recommendation

**Total: 15–30 person-days** for a working, CI-verified, distributable Windows build,
by one experienced Rust developer who knows this codebase. The spread is dominated by
the capture work (Phases 1–2, 7–13 days of the total) and how cleanly the silence /
device-change / format questions resolve in the Phase-1 spike. The low end assumes
`cpal` or the `wasapi` crate fits cleanly and silence handling is a small adaptation;
the high end assumes a hand-rolled windows-rs backend and meaningful rework of the
watchdog's idle interpretation. Packaging and signing add real-world (not technical)
time depending on the distribution and signing decisions in §5.

**Recommendation: DEFER, but keep the door open.** The port is *cheap and low-risk*
precisely because the architecture already isolated the platform boundary — that is
exactly why it does not need to be rushed. There is no architectural debt accruing by
waiting, and the seam will not rot. Today's value is higher in macOS polish and in
the public release than in a second platform with no demonstrated user base.

**Criteria that would flip this to GO:**

- Demonstrated Windows demand (issues/requests, or a target community that is
  Windows-first).
- A contributor with Windows audio experience willing to own Phases 1–2 (capture is
  the part where prior WASAPI experience compresses the estimate most).
- A decision to broaden the project's reach where being macOS-only is the binding
  constraint on adoption.

If any of those holds, start at **Phase 1** (the capture spike): it is the smallest
amount of work that retires the largest amount of uncertainty, and it can be done
without committing to the full port.

---

## Sources

Windows audio capture:
- [WASAPI loopback recording — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
- [AUDCLNT_STREAMFLAGS_XXX constants — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audclnt-streamflags-xxx-constants)
- [Application loopback audio capture sample — Microsoft Learn](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/)
- [AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS — Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params)
- [WASAPI loopback capture sample (silence/discontinuity behavior)](https://matthewvaneerde.wordpress.com/2008/12/16/sample-wasapi-loopback-capture-record-what-you-hear/)
- [cpal — Support WASAPI loopback (issue #251)](https://github.com/RustAudio/cpal/issues/251) and [PR #339](https://github.com/RustAudio/cpal/pull/339)
- [`wasapi` crate — docs.rs](https://docs.rs/wasapi)

Rendering / windowing / UI:
- [wgpu — repository and backend support](https://github.com/gfx-rs/wgpu)
- [Make DX12 the default API on Windows (wgpu #2719)](https://github.com/gfx-rs/wgpu/issues/2719)
- [Backends in wgpu — docs.rs](https://docs.rs/wgpu/latest/wgpu/struct.Backends.html)
- [winit 0.30 with wgpu — discussion](https://github.com/rust-windowing/winit/discussions/3667)
- [winit features (platform support) — docs.rs](https://docs.rs/crate/winit/latest/source/FEATURES.md)

Packaging / signing:
- [Code signing options for Windows app developers — Microsoft Learn](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options)
- [SmartScreen reputation for Windows app developers — Microsoft Learn](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation)
- [EV vs OV reputation (Microsoft Q&A)](https://learn.microsoft.com/en-us/answers/questions/417016/reputation-with-ov-certificates-and-are-ev-certifi)
- [Azure Artifact Signing (Trusted Signing) — quickstart](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart) and [individual-developer eligibility/restrictions](https://learn.microsoft.com/en-us/azure/artifact-signing/faq)

CI:
- [GitHub Actions GPU hosted runners — Changelog](https://github.blog/changelog/2024-07-08-github-actions-gpu-hosted-runners-are-now-generally-available/)
- [DX12 WARP fallback rendering quirk (wgpu #2503)](https://github.com/gfx-rs/wgpu/issues/2503)
