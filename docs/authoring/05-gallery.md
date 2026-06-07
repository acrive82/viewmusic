# 5. Gallery

> The fifteen shipped built-ins, explained — what each one does, what drives it, the
> techniques worth stealing, and the settings worth turning. Read these like worked
> examples: every claim here matches the actual file in
> [`assets/builtin-artifacts/`](../../assets/builtin-artifacts/).

Each entry follows the same schema: **What you see**, **Reacts to**, **Key
techniques** (each linking the recipe or reference section that teaches it), and
**Settings worth playing with**. The first five are the originals; the ten after them
came later and lean harder on the recipes in [chapter 4](./04-recipes.md).

Want to learn a technique? Find a built-in that uses it here, open its file, and read
it next to the recipe it links to.

---

## The originals

### Spectrum Bars

`spectrum-bars` —
[`spectrum-bars.artifact.json`](../../assets/builtin-artifacts/spectrum-bars.artifact.json)

**What you see.** A classic Winamp-style analyzer: a row of log-spaced bars whose
heights track the spectrum, with a thin waveform trace floating above and a soft beat
tint on the background.

**Reacts to.** `band(u)` per bar (height and color), `energy` (the trace's thickness
and brightness), and a smoothed `flash` var for the background tint.

**Key techniques.**

- Spectrum-mapped [instanced](./03-scenes-layers-settings.md) bars — element `u`
  feeds `band(u)` so each bar samples its own slice of the spectrum.
- A waveform [polyline](./03-scenes-layers-settings.md) using `wave(u)`.
- A smoothed background tint, `max(flash * 0.88, beat)`, driving a low-brightness
  field rather than a strobe — the [flash-free beats](./04-recipes.md#46-flash-free-beats)
  idea (the beat is smoothed before it touches brightness, and capped at 0.25).
- [Feedback trails](./04-recipes.md#43-feedback-trails) (decay 0.85) for bar
  persistence.

**Settings worth playing with.** `bars` (8–96 bars), `style` (thin vs wide), `glow`
toggle, `hueShift`, and the `baseColor` beat tint.

---

### Radial Pulse

`radial-pulse` —
[`radial-pulse.artifact.json`](../../assets/builtin-artifacts/radial-pulse.artifact.json)

**What you see.** A ring of spectrum-driven shapes pulsing outward from the center,
slowly rotating, with the beat punching the radius.

**Reacts to.** `band(u)` per element (radius offset, size, color), `energy` (gates the
spectrum swell and feeds the spin speed), and a smoothed `pulse` var on the beat.

**Key techniques.**

- The [true-circles idiom](./04-recipes.md#41-true-circles): every x divided by
  `aspect` (`cos(...) / aspect`, `w = .../aspect`) so the ring stays round.
- A smoothed `pulse` (`max(pulse * 0.9, beat)`) nudges the radius and brightness — the
  beat felt through geometry, not a flash ([flash-free beats](./04-recipes.md#46-flash-free-beats)).
- [Calm on silence](./04-recipes.md#45-calm-on-silence): the spectrum swell is gated
  by `smoothstep(0.02, 0.15, energy)`.
- [Feedback trails](./04-recipes.md#43-feedback-trails) (decay 0.8).

**Settings worth playing with.** `spokes` (8–128), `baseRadius`, `shapeStyle` (dots
vs spokes), `hueSweep`, and the center `tint`.

---

### Oscilloscope

`oscilloscope` —
[`oscilloscope.artifact.json`](../../assets/builtin-artifacts/oscilloscope.artifact.json)

**What you see.** A glowing waveform trace of the raw audio window — either one
centered scope or a dual mirrored pair — thickening with energy and shifting color on
the beat.

**Reacts to.** `wave(u)` (the trace shape), `energy` (thickness, amplitude,
brightness), and a smoothed `glow` var on the beat.

**Key techniques.**

- A waveform [polyline](./03-scenes-layers-settings.md) — the canonical `wave(u)` use.
- A smoothed `glow` (`max(glow * 0.9, beat)`) that adds *thickness* and a hue offset,
  not a brightness flash ([flash-free beats](./04-recipes.md#46-flash-free-beats)).
- [Calm on silence](./04-recipes.md#45-calm-on-silence): amplitude scaled by
  `smoothstep(0.02, 0.2, energy)`.
- A toggle-gated [feedback decay](./04-recipes.md#43-feedback-trails)
  (`settings.trails * 0.86`) and a `visible`-gated second/third trace for dual mode.

**Settings worth playing with.** `mode` (centered vs dual mirror), `amplitude`,
`lineWeight`, the `glowColor`, and the `trails` toggle.

---

### Particle Burst

`particle-burst` —
[`particle-burst.artifact.json`](../../assets/builtin-artifacts/particle-burst.artifact.json)

**What you see.** Sparks explode from the center on every beat and coast outward,
fading as they fly — fireworks with optional gravity.

**Reacts to.** `beat` (launches a burst via `beat_count` / `lastBeatT`); `energy` is
deliberately *not* used for the motion, which is purely (index, age).

**Key techniques.**

- The [closed-form particle idiom](./04-recipes.md#42-closed-form-particles) in full:
  `lastBeatT` launch-time var, `rand(i + beat_count * 1000)` re-rolled directions,
  position = `f(rand(i), t - lastBeatT)`.
- An optional gravity arc via a `pow(age, 2)` term on `y`.
- The dead-spark cut with `step(0, life - age)` so silence leaves nothing flying.
- Additive blend + [feedback](./04-recipes.md#43-feedback-trails) (decay 0.9) for a
  glow.

**Settings worth playing with.** `particles` (8–512), `spread`, `life`, the `gravity`
toggle, and `sparkColor`.

---

### Color Field

`color-field` —
[`color-field.artifact.json`](../../assets/builtin-artifacts/color-field.artifact.json)

**What you see.** A full-canvas plasma whose waves are steered by the frequency bands;
in silence it falls to a calm dark wash you can tint.

**Reacts to.** `low` / `mid` / `high` (steer separate sine terms), `energy` (gates
brightness and feedback), and a smoothed `warmth` var (`low − high`).

**Key techniques.**

- A [field](./03-scenes-layers-settings.md) layer using `x`, `y` cell coordinates —
  the canonical plasma.
- [Calm on silence](./04-recipes.md#45-calm-on-silence) done explicitly: brightness is
  `smoothstep(0.02, 0.12, energy) * (lively) + (1 - gate) * (idle wash)`, blending a
  dim tinted floor in as the gate closes.
- Band-steered motion: bands shift the sine phases, and a `phase` var advances at
  `dt * flow * (0.2 + 1.5 * energy)` — gated motion.
- Energy-reactive [feedback](./04-recipes.md#43-feedback-trails)
  (`0.82 + 0.12 * energy`).

**Settings worth playing with.** `palette` (aurora / magma / mono), `scale`, `flow`
speed, `hueShift`, and the `calmColor` idle wash.

---

## The newer set

### Aurora Waves

`aurora-waves` —
[`aurora-waves.artifact.json`](../../assets/builtin-artifacts/aurora-waves.artifact.json)

**What you see.** Northern lights: three or four slow ribbons of light drifting across
a dim sky, each fed by its own frequency band (low ribbon low on screen, high near the
top), fused by heavy feedback into a smooth veil.

**Reacts to.** Per-band smoothed vars `low_s` / `mid_s` / `high_s` (each ribbon's
amplitude and brightness), and `energy` (drift speed, glow thickness, feedback).

**Key techniques.**

- Multi-sine [polyline](./03-scenes-layers-settings.md) ribbons with stable
  per-ribbon phases from `rand(1)`, `rand(11)`, `rand(21)` — see
  [rand stability](./02-formula-language.md).
- Per-band [smoothing vars](./03-scenes-layers-settings.md)
  (`low_s * 0.9 + low * 0.1`) so amplitudes glide.
- [Calm on silence](./04-recipes.md#45-calm-on-silence): every ribbon's brightness is
  `smoothstep(0.02, 0.12, energy) * (...) + tiny_floor`.
- Heavy energy-reactive [feedback](./04-recipes.md#43-feedback-trails)
  (`0.9 + 0.08 * energy`) to fuse the strokes; a `visible`-gated fourth ribbon.

**Settings worth playing with.** `ribbons` (3 vs 4), `glow`, `baseHue`, `drift` speed,
and the `veil` sky color.

---

### Bass Tunnel

`bass-tunnel` —
[`bass-tunnel.artifact.json`](../../assets/builtin-artifacts/bass-tunnel.artifact.json)

**What you see.** An infinite tunnel of concentric ring outlines receding toward the
center, scrolling inward and breathing with the bass.

**Reacts to.** A smoothed `lowS` var (ring scale, thickness, hue), and `energy` (scroll
speed via the `depth` var, brightness gate).

**Key techniques.**

- The [ring idiom](./04-recipes.md#a-note-on-rings): each ring is a **closed
  polyline** on a circle (a true outline, not a filled disc), with the
  [aspect correction](./04-recipes.md#41-true-circles) `cos(...) / aspect`.
- A scrolling `depth` var plus `fract((offset + depth) / rings)` to recycle rings
  inward — closed-form looping.
- [Calm on silence](./04-recipes.md#45-calm-on-silence): brightness gated by
  `smoothstep(0.02, 0.12, energy)`; bass smoothed into `lowS` so the breathing is
  fluid, never a beat flash.
- Settings-driven [feedback](./04-recipes.md#43-feedback-trails) (`decay = settings.glow`).

**Settings worth playing with.** `rings` (4–14; >7 reveals a seventh ring),
`pulse` depth, `speed`, `glow`, and the depth `tint`.

---

### Breathing Grid

`breathing-grid` —
[`breathing-grid.artifact.json`](../../assets/builtin-artifacts/breathing-grid.artifact.json)

**What you see.** A zen grid: a soft field of muted color cells breathing with a slow
spatial wave, overlaid by a lattice of small dots that pulse with the mid band. The
calmest visual of the set — alpha blend, no additive, no flashes.

**Reacts to.** A heavily smoothed `energy_s` var (the breath depth) and a smoothed
`mid_s` var (dot size and brightness).

**Key techniques.**

- A [field](./03-scenes-layers-settings.md) whose breath depth is scaled by *smoothed*
  energy (`energy_s * 0.93 + energy * 0.07`) — a long, gentle attack.
- An [instanced](./03-scenes-layers-settings.md) dot lattice indexed by modulo math
  (`i % density`, `floor(i / density)`) with the
  [true-circles](./04-recipes.md#41-true-circles) `w = .../aspect` correction.
- [Calm on silence](./04-recipes.md#45-calm-on-silence) by design: dot brightness
  gated by `smoothstep`, and `feedback.decay` is **0** — no trails, deliberately still.

**Settings worth playing with.** `density` (4–16 — drives both the cell wave and the
dot count as density²), `breath` speed, `hue`, `dotSize`, and the idle `tint`.

---

### DNA Helix

`dna-helix` —
[`dna-helix.artifact.json`](../../assets/builtin-artifacts/dna-helix.artifact.json)

**What you see.** A rotating double helix: two instanced-circle strands crossing
vertically, with faux depth from size/alpha, and rungs connecting them every few
beads. Spins faster with midrange energy.

**Reacts to.** `band(u)` per bead (local radius swell), `mid` (rotation speed via the
`phase` var), `energy` (brightness gate), and a smoothed `pulse` var (rung thickness).

**Key techniques.**

- Two [instanced](./03-scenes-layers-settings.md) strands offset by `+ pi` in the
  phase so they cross; faux z-depth from `0.5 + 0.5 * cos(phase)` on size and alpha.
- The [true-circles](./04-recipes.md#41-true-circles) `x = .../aspect`, `w = .../aspect`
  correction on every bead.
- [Calm on silence](./04-recipes.md#45-calm-on-silence): bead size, color, and rung
  brightness all gated by `smoothstep(0.02, 0.12, energy)`; spin speed scaled by `mid`.
- A third instanced rung layer whose width follows `abs(sin(angle + phase))` so the
  rungs widen and narrow as the strands cross.

**Settings worth playing with.** `rotSpeed`, `turns` (1–8 helix turns), and the two
strand colors `strandA` / `strandB`.

---

### Kaleido Petals

`kaleido-petals` —
[`kaleido-petals.artifact.json`](../../assets/builtin-artifacts/kaleido-petals.artifact.json)

**What you see.** A kaleidoscopic flower: N triangle petals in a ring pointing
outward, with a smaller counter-rotating inner ring for depth; petals breathe with the
spectrum and the beat adds a tiny bloom to their *length* only.

**Reacts to.** `band(u)` per petal (length, width, color), `energy` (spin speed,
brightness gate), and a smoothed `bloomS` var on the beat.

**Key techniques.**

- A radial [instanced](./03-scenes-layers-settings.md) ring with per-petal `rot = u * tau + spin + pi/2`
  to aim petals outward, with the [aspect correction](./04-recipes.md#41-true-circles).
- A textbook [flash-free beat](./04-recipes.md#46-flash-free-beats): `bloomS`
  (`max(bloomS * 0.9, beat * 0.5)`) adds to **petal length only** — `settings.bloom * bloomS`
  on the radius and `h` — never to a color channel.
- [Calm on silence](./04-recipes.md#45-calm-on-silence): petal extent and brightness
  gated by `smoothstep(0.02, 0.12, energy)`, folding petals to a small bud in silence.
- A second counter-rotating inner ring sampling `band(0.3 + 0.5 * u)`.

**Settings worth playing with.** `petals` (5–48), `bloom` intensity, `hueRange`,
`spinSpeed`, and the base `tint`.

---

### Liquid Spectrum

`liquid-spectrum` —
[`liquid-spectrum.artifact.json`](../../assets/builtin-artifacts/liquid-spectrum.artifact.json)

**What you see.** A liquid horizon: two mirrored polylines trace a smoothed spectrum
curve that sloshes, with a translucent body filling between them, bobbing gently with
the low end. An optional mirror reflects the curve below.

**Reacts to.** A var-smoothed `amp` (the curve amplitude), a smoothed `bob` var on
`low` (vertical bob), `band(u)` (curve shape and color), and `energy` (brightness gate).

**Key techniques.**

- Spectrum [polylines](./03-scenes-layers-settings.md) with neighbor-averaged sampling
  (`0.5 * band(u) + 0.5 * band(clamp(u + 0.05, 0, 1))`) for a smooth curve.
- Asymmetric-feeling [smoothing](./03-scenes-layers-settings.md) via
  `mix(energy, amp, settings.smoothness)` — a settings-tunable lag for the "fluid" feel.
- [Calm on silence](./04-recipes.md#45-calm-on-silence): brightness gated by
  `smoothstep`, flattening the liquid to a dim line.
- A `visible`-gated mirror layer and amplitude-reactive
  [feedback](./04-recipes.md#43-feedback-trails) (`0.78 + 0.12 * amp`).

**Settings worth playing with.** `amplitude`, `smoothness` (0 = snappy, ~0.95 =
glassy), the `hue` color, and the `mirror` toggle.

---

### Neon Gauges

`neon-gauges` —
[`neon-gauges.artifact.json`](../../assets/builtin-artifacts/neon-gauges.artifact.json)

**What you see.** Retro VU meters: three glowing 180° arc gauges (low / mid / high)
built from rows of instanced segments, each with a needle and tick marks, that leap to
their band level and ease back down. Lay them in a row or a triangle.

**Reacts to.** Three asymmetric-smoothed vars `low_g` / `mid_g` / `high_g` (the
fill level and needle angle of each gauge), and `energy` (brightness gate).

**Key techniques.**

- The [peak-hold / asymmetric smoothing](./04-recipes.md#44-peak-hold--asymmetric-smoothing)
  idiom — this is its reference built-in: `if(low > low_g, low, low_g * settings.peakHold)`,
  fast attack, slow release.
- Arc gauges laid out with [instanced](./03-scenes-layers-settings.md) segments on a
  `cos(pi - pi * i / 11)` / `sin(...)` arc, with the
  [aspect correction](./04-recipes.md#41-true-circles).
- `step((i % 20) / 19, low_g)` to light segments up to the held level.
- No beat flash: the panel only moves through the smoothed levels
  ([flash-free beats](./04-recipes.md#46-flash-free-beats)); brightness gated by
  `smoothstep(0.02, 0.12, energy)`.

**Settings worth playing with.** `peakHold` (0.8–0.99 release factor — the heart of
the effect), `layout` (row vs triangle), `gaugeColor`, and `brightness`.

---

### Pixel Rain

`pixel-rain` —
[`pixel-rain.artifact.json`](../../assets/builtin-artifacts/pixel-rain.artifact.json)

**What you see.** Digital rain: columns of glyph-like rects streaming downward, each
column with a fading trail behind a bright falling head. Loud frequencies pour faster
and brighter — but the brightness follows the band smoothly, never the beat.

**Reacts to.** `band(column_x)` per column (fall speed and brightness), and a smoothed
`energy_s` var (overall brightness gate).

**Key techniques.**

- Two [instanced](./03-scenes-layers-settings.md) layers — the trail body
  (`columns * 18` cells) and the bright heads — indexed by column via
  `floor(i / 18)` and row via `i % 18`.
- Per-column stable randomness from `rand(floor(i / 18))` for speed and offset
  ([rand stability](./02-formula-language.md)).
- **Luminance follows the band smoothly, never the beat**
  ([flash-free beats](./04-recipes.md#46-flash-free-beats)): brightness is
  `band(...) * smoothstep(0.02, 0.12, energy_s)` — the `beat` input is never read in a
  single formula here.
- A trail falloff via `pow(1 - position, 18 / trail)` and short
  [feedback](./04-recipes.md#43-feedback-trails) (decay 0.55).

**Settings worth playing with.** `columns` (8–64), `fallSpeed`, `hue`, `trail` length,
and the `headColor` glow.

---

### Spectrum Galaxy

`spectrum-galaxy` —
[`spectrum-galaxy.artifact.json`](../../assets/builtin-artifacts/spectrum-galaxy.artifact.json)

**What you see.** A spiral galaxy: hundreds of stars along two-to-four logarithmic
arms, a field of background dust, and a soft glowing core. The disc rotates slowly and
the arms shimmer outward with the music.

**Reacts to.** `band(u)` per star (radius perturbation, size, color), a smoothed
`energy_s` var (the core glow and feedback), and `energy` (rotation, brightness gate).

**Key techniques.**

- A [field](./03-scenes-layers-settings.md) core glow driven by **smoothed** energy
  (`energy_s`, never the beat) with a Gaussian `exp(-(x²·aspect² + y²)·7)` falloff.
- Spiral arms from [instanced](./03-scenes-layers-settings.md) stars: arm =
  `i % (2 + arms)`, radius from `u` plus a `band(u)` perturbation, all with the
  [aspect correction](./04-recipes.md#41-true-circles).
- Per-star color jitter via `mix(starColor, 1, 0.5 * rand(i + 70))`.
- [Calm on silence](./04-recipes.md#45-calm-on-silence) throughout and energy-reactive
  [feedback](./04-recipes.md#43-feedback-trails) (`0.86 + 0.08 * energy_s`).

**Settings worth playing with.** `arms` (2 / 3 / 4), `spin` speed, `core` intensity,
and `starColor`.

---

### Starfield Warp

`starfield-warp` —
[`starfield-warp.artifact.json`](../../assets/builtin-artifacts/starfield-warp.artifact.json)

**What you see.** Flying through space: stars stream outward from the center along
radial trajectories, leaving streaks. The warp speeds up with energy, and a beat gives
a gentle warp kick. Silence settles the drift to a crawl.

**Reacts to.** `energy` (warp speed via the `warp` var, brightness gate), and a
smoothed `kick` var on the beat.

**Key techniques.**

- The [closed-form particle idiom](./04-recipes.md#42-closed-form-particles): each
  star takes a stable angle and launch radius from `rand(i)` / `rand(i + 300)`;
  distance grows with an accumulated `warp` var — position is `f(rand(i), warp)`.
- A [flash-free beat](./04-recipes.md#46-flash-free-beats): a smoothed `kick`
  (`max(kick * 0.9, beat * 0.5)`) nudges **warp speed and star size** — geometry, not
  color.
- [Calm on silence](./04-recipes.md#45-calm-on-silence): brightness and alpha gated by
  `smoothstep(0.02, 0.12, energy)`; warp speed scaled by energy.
- Settings-driven [feedback trails](./04-recipes.md#43-feedback-trails)
  (`decay = settings.trail`) to turn motion into streaks.

**Settings worth playing with.** `stars` (32–1200), `warpSpeed`, `trail` length, and
the star `tint`.

---

<sub>Prev: [4. Recipes](./04-recipes.md) ·
Next: [6. Troubleshooting](./06-troubleshooting.md) ·
[Table of contents](./README.md)</sub>
