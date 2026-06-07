# 4. Recipes

> Six proven patterns you can copy, paste, and adapt — each with the problem it
> solves, the formulas explained line by line, a runnable file, variations, and the
> traps to avoid.

Every recipe here is a complete artifact under
[`examples/`](./examples/). Drop any one into your artifacts folder (see
[chapter 6](./06-troubleshooting.md)) and it loads and reacts as described — the
manual's automated test loads all of them on every build, so they never drift.

**Sources of truth.** Four of these idioms are defined normatively in the
[artifact contract](../reference/artifact-contract.md);
those sections rule, and these recipes only teach and exemplify them. The remaining
two — **flash-free beats** and **peak-hold smoothing** — are *conventions* the
built-ins follow, and this chapter is their first written home. Where that is the
case the recipe says so and states the rule crisply.

| Recipe | Source of truth |
|---|---|
| [True circles](#41-true-circles) | contract §2.3 (circle idiom) |
| [Closed-form particles](#42-closed-form-particles) | contract §6.1 (particle idiom) |
| [Feedback trails](#43-feedback-trails) | contract §8 (feedback) |
| [Peak-hold / asymmetric smoothing](#44-peak-hold--asymmetric-smoothing) | **this manual** (convention) |
| [Calm on silence](#45-calm-on-silence) | contract §3 (audio semantics) |
| [Flash-free beats](#46-flash-free-beats) | **this manual** (convention) |

---

## 4.1 True circles

**Source of truth:** contract
[§2.3 "Circle idiom"](../reference/artifact-contract.md#23-inputs-read-only-identifiers).

### The problem

The coordinate system runs `x ∈ −1..1` left→right and `y ∈ −1..1` bottom→top
*independent of window size*. On a wide window, one x-unit is physically wider than
one y-unit. So the naive ring `(cos a, sin a)` draws an **ellipse**, not a circle —
it stretches horizontally exactly as much as the window does.

### The pattern

Divide every x-distance by `aspect` (= width / height). The runnable example
([`recipe-true-circles.artifact.json`](./examples/recipe-true-circles.artifact.json))
draws a rotating ring of `circle` instances:

```json
"x": "settings.radius * cos(u * tau + spin) / aspect",
"y": "settings.radius * sin(u * tau + spin)",
"w": "(settings.dotSize * (0.6 + 0.6 * band(u))) / aspect",
"h": "settings.dotSize * (0.6 + 0.6 * band(u))"
```

Line by line:

- `u * tau` — each instance's `u` runs 0..1 across the count, so `u * tau` sweeps a
  full turn (`tau` = 2π). `+ spin` rotates the whole ring over time.
- `cos(...) / aspect`, `sin(...)` — the **position** correction: x is squeezed by
  `aspect` so a unit circle in math units renders as a true circle in pixels.
- `w = d / aspect`, `h = d` — the **size** correction, the same idea applied to each
  dot so it stays visually square instead of being stretched into an oval.

That `/ aspect` on *both* the position's x and the width is the whole recipe.

### Variations

- **A ring outline instead of dots.** Filled `circle` shapes are discs. For a thin
  *ring* outline, use a closed `polyline` on the same circle — see
  [§4.1's sibling, the ring idiom](#a-note-on-rings) below.
- **Pulse the radius on audio.** Replace `settings.radius` with
  `settings.radius + 0.2 * low` for a bass-driven breathing ring (keep it smoothed —
  see [flash-free beats](#46-flash-free-beats)).
- **Per-dot spectrum size.** The example already swells each dot with `band(u)`, so
  the ring shimmers with the spectrum.

### A note on rings

To draw a ring *outline* (not a disc), lay a **closed polyline** on the circle —
the contract's
[ring idiom, §6.2](../reference/artifact-contract.md#62-polyline--connected-strip-oscilloscope-curves):
`x = r * cos(u * tau) / aspect`, `y = r * sin(u * tau)`, `closed: true`. The same
aspect correction applies. [Bass Tunnel](./05-gallery.md#bass-tunnel) is built
from a stack of these.

### Pitfalls

- **Forgetting `aspect` on the width.** A common half-fix corrects the position but
  not the size, leaving round positions with oval dots. Correct *both*.
- **Dividing y by aspect.** Only x stretches with a wide window. Divide x, never y.
- **`count` reaching 0.** `count` is clamped to a floor of 1, so a layer never
  vanishes by count alone; gate visibility with `visible` instead (contract §6.1).

---

## 4.2 Closed-form particles

**Source of truth:** contract
[§6.1 "Particle idiom"](../reference/artifact-contract.md#61-instanced--n-shapes-bars-particles-radial-patterns).

### The problem

Instanced elements carry **no state between frames** — each frame, element `i` is
recomputed from scratch. There is no persistent velocity or lifetime; per-element
physics simulation is intentionally not expressible (purity, contract §9). So how do
you make a beat-triggered spark burst?

### The pattern

Design each particle as a **closed-form trajectory of (index, age)**: a pure
function of its stable random direction and how long ago its burst launched. The
runnable example
([`recipe-particles.artifact.json`](./examples/recipe-particles.artifact.json))
uses exactly the three pieces the contract calls out.

**1. A launch-time var** records when the current burst started:

```json
"vars": { "lastBeatT": { "init": "0", "frame": "if(beat >= 1, t, lastBeatT)" } }
```

On the frame a beat fires (`beat` snaps to 1.0), `lastBeatT` becomes the current
clock `t`; otherwise it holds its previous value. So `t - lastBeatT` is each spark's
**age** in seconds since the burst.

**2. Stable per-particle randomness, re-rolled per burst:**

```
rand(i + beat_count * 1000)          → this spark's direction angle (0..1 → ×tau)
rand(i + 100 + beat_count * 1000)    → this spark's speed (independent draw)
rand(i + 200 + beat_count * 1000)    → this spark's size (independent draw)
```

`rand(k)` is a *stateless hash*: the same `k` gives the same value every frame, so a
particle's direction is rock-steady during a burst. Folding `i` in gives each
particle its own value; folding `+ 100` / `+ 200` in gives **independent** draws for
speed and size; folding `beat_count * 1000` in **re-rolls** every value on each new
beat, so each burst flies a fresh pattern.

**3. Position = f(direction, age):**

```json
"x": "cos(rand(i + beat_count * 1000) * tau) * settings.spread * (0.4 + 0.6 * rand(i + 100 + beat_count * 1000)) * clamp((t - lastBeatT) / settings.life, 0, 1) / aspect",
```

- `cos(angle * tau)` — the spark's direction (note `/ aspect` keeps the burst round).
- `* settings.spread * (0.4 + 0.6 * speed)` — how far it can travel, spread plus a
  per-spark speed multiplier.
- `* clamp((t - lastBeatT) / settings.life, 0, 1)` — **age normalized to 0..1**: the
  spark travels from center (age 0) to full distance (age = `life`), then stops.

The color fades the same way and the alpha cuts to 0 past `life` with
`step(0, settings.life - (t - lastBeatT))`, so dead sparks disappear and silence (no
new beats) leaves nothing flying.

### Variations

- **Gravity.** Subtract a `pow(age, 2)` term from `y` for a fountain arc (this is
  what [Particle Burst](./05-gallery.md#particle-burst) does with its Gravity toggle).
- **Continuous emission.** Replace the beat-gated launch time with `t` itself and a
  per-particle phase offset for a steady stream instead of bursts.
- **Color by speed.** Tint faster sparks differently using the same speed draw.

### Pitfalls

- **Expecting a simulation.** You cannot accumulate per-particle velocity. If you
  find yourself wanting `velocity[i] += ...`, step back and express position as a
  function of age instead.
- **Re-rolling per frame.** `rand(i + t)` changes every frame and makes particles
  jitter. Re-roll on a *discrete* event (`beat_count`), not on continuous `t`.
- **Forgetting to kill dead sparks.** Without the `step(...)` alpha cut, sparks
  freeze at full distance instead of vanishing.

---

## 4.3 Feedback trails

**Source of truth:** contract
[§8 "Feedback (trails)"](../reference/artifact-contract.md#8-feedback-trails).

### The problem

You want glowing streaks and persistence — movers that leave a tail — without
storing past positions (you can't; there's no history in the language).

### The pattern

Scene-level feedback does it for free: each frame the *previous frame's image* is
multiplied by `decay` (clamped 0..0.99) before this frame's layers draw. Additively
blended movers then paint onto a faded copy of where they were. The runnable example
([`recipe-trails.artifact.json`](./examples/recipe-trails.artifact.json)) orbits a
few additive dots over a feedback layer:

```json
"feedback": { "decay": "settings.persistence + (0.97 - settings.persistence) * energy * 0.5" }
```

- A constant base `settings.persistence` sets the resting trail length.
- The `+ (0.97 - persistence) * energy * 0.5` term lengthens trails toward the 0.97
  ceiling as the music gets louder, so busy passages streak more.

The movers themselves use `"blend": "add"` so each new dot *adds* light to the
decaying trail rather than covering it, building luminous ribbons.

### Variations

- **Crisp vs smeared.** `decay` near 0 = no trail (crisp); near 0.97 = long ghosting.
- **Audio-reactive decay.** Tie `decay` to `low` for bass-driven smear, or to a
  toggle: `"settings.trails * 0.86"` turns trails on/off (Oscilloscope does this).
- **Pair with motion.** Trails shine on anything that moves smoothly — orbits,
  warps, scopes. [Starfield Warp](./05-gallery.md#starfield-warp) uses trails to turn
  moving stars into streaks.

### Pitfalls

- **`decay ≥ 1`.** The contract clamps to 0.99; a value of 1 would never fade and the
  canvas would saturate to white. The clamp protects you, but design under 0.99.
- **Alpha-blend movers over feedback.** With `"blend": "alpha"` a bright opaque mover
  *covers* the trail instead of glowing into it. Use `"add"` for streaks.
- **Cost.** Feedback is one offscreen pass and is the first thing dropped under the
  performance ladder (contract §8) — fine to use, just know it isn't free.

---

## 4.4 Peak-hold / asymmetric smoothing

**Codified here for the first time.** This is a convention demonstrated by
[Neon Gauges](./05-gallery.md#neon-gauges), not a contract rule — this section is its
written home.

### The rule (crisp)

> A meter should **leap up instantly** to a new peak and **ease back down slowly** —
> fast attack, slow release. Hold the peak; release it gently. And because it changes
> luminance, the release must be *smooth* (a per-frame multiply), never a flash.

### The problem

Plain symmetric smoothing (`s = s * 0.9 + low * 0.1`) lags on the way *up* too, so a
meter feels mushy and never quite reaches the peak. A VU meter wants the opposite:
snap to the peak, then decay.

### The pattern

One conditional var does it. The runnable example
([`recipe-peak-hold.artifact.json`](./examples/recipe-peak-hold.artifact.json)):

```json
"vars": { "hold": { "init": "0", "frame": "if(low > hold, low, hold * settings.release)" } }
```

Read it as: **if the incoming band beats the held value, jump straight to it;
otherwise multiply the held value by the release factor** (e.g. 0.96).

- `low > hold` → `low` — **instant attack**: a louder low band sets the meter
  immediately, no lag.
- else → `hold * settings.release` — **slow release**: when the band falls, the meter
  eases down by a fixed fraction per frame (0.96 ⇒ a gentle ~4%/frame decay).

The segments light up to the held level with `step(u, hold)` — each segment whose
position `u` is below `hold` is lit:

```json
"r": "settings.litColor_r * (0.08 + 0.92 * step(u, hold)) * (0.3 + 0.7 * smoothstep(0.02, 0.12, energy))"
```

Note the brightness is still energy-gated (the `smoothstep(...)` factor) and the lit
fraction `step(u, hold)` is driven by the *smoothed* `hold`, never by the raw beat —
so the panel is calm in silence and changes luminance smoothly.

### Variations

- **Independent per-band gauges.** Run three vars `low_g`, `mid_g`, `high_g`, each
  with its own `if(...)` — exactly what [Neon Gauges](./05-gallery.md#neon-gauges)
  does with a needle per band.
- **Adjustable release.** Expose the release factor as a setting (`peakHold` /
  `release`); higher = longer hold.
- **A peak dot.** Draw a single bright marker at radius/height `hold` for a classic
  "peak indicator" cap above the bar.

### Pitfalls

- **Release ≥ 1.** `hold * 1.0` never falls and the meter sticks at max. Keep release
  strictly below 1 (the example caps the setting at 0.99).
- **Attack on the wrong side.** `if(low < hold, ...)` inverts it into slow-attack /
  fast-release — a meter that lags up and snaps down, which looks wrong.
- **Wiring the beat into color.** Tempting to flash the panel on a beat; don't — drive
  geometry/levels through the smoothed `hold`. See [flash-free beats](#46-flash-free-beats).

---

## 4.5 Calm on silence

**Source of truth:** contract
[§3 "Audio semantics"](../reference/artifact-contract.md#3-audio-semantics).

### The rule (crisp)

> During silence the app feeds **zeroed features** (`energy`, bands, `band()`,
> `wave()`, `beat` all → 0) — **but `t` and `dt` keep advancing**. So gate motion and
> brightness by the *audio*, not by time, or your scene will keep animating on `t`
> alone over dead silence.

### The problem

If brightness or speed depend only on `t` (`v = 0.5 + 0.5 * sin(t)`), the scene never
rests — it churns identically whether music is playing or the room is silent. The
project's first golden rule is **calm silence**.

### The pattern

Multiply every brightness term, and scale every motion speed, by an energy gate. The
clean idiom is `smoothstep(0.02, 0.12, energy)`: it is 0 below ~0.02 energy, ramps up,
and reaches 1 by ~0.12 — a soft fade-in as sound arrives. The runnable example
([`recipe-calm-silence.artifact.json`](./examples/recipe-calm-silence.artifact.json))
applies it to a deliberately busy scene (plasma field + spectrum swarm):

```json
"v": "(settings.floor_r + settings.floor_g + settings.floor_b) * 0.2 + smoothstep(0.02, 0.12, energy) * (0.2 + 0.8 * abs(sin(x * settings.scale + phase) * cos(y * settings.scale - phase * 0.7)))"
```

- The first term is a tiny constant **idle floor** (a dim color from a setting) so the
  scene isn't pure black in silence — a resting glow, not nothing.
- The second term, the lively plasma, is **multiplied by the energy gate**, so it only
  appears when there's sound and vanishes smoothly when sound stops.

Motion is gated too, so the swarm settles instead of spinning forever:

```json
"phase": { "init": "0", "frame": "phase + dt * (0.1 + 1.5 * energy)" }
```

- The `0.1` keeps a barely-perceptible drift even at rest (optional — set it to 0 for
  a full freeze); the `+ 1.5 * energy` is the real motion, gated by loudness.

### Variations

- **Hard floor of 0.** Drop the idle-floor term for a scene that goes fully dark in
  silence.
- **Per-band gates.** Gate different layers on `low` / `mid` / `high` for parts that
  wake at different frequencies.
- **Smoothed energy.** For an even gentler fade, gate on a smoothed energy var
  (`energy_s`) instead of raw `energy` — see [§4.6](#46-flash-free-beats).

### Pitfalls

- **`step` instead of `smoothstep`.** A hard `step(0.02, energy)` pops on/off at the
  threshold instead of fading — jarring on quiet passages. Prefer `smoothstep`.
- **Gating only brightness, not motion.** A frozen-but-spinning scene still betrays
  the silence. Scale speeds by `energy` too.
- **Animating on `t` for "ambiance."** A little `t` drift is fine; a full animation on
  `t` defeats the rule. When in doubt, multiply by `energy`.

---

## 4.6 Flash-free beats

**Codified here for the first time.** This is a convention demonstrated by
[Kaleido Petals](./05-gallery.md#kaleido-petals),
[Starfield Warp](./05-gallery.md#starfield-warp), and others — not a contract rule.
This section is its written home.

### The rule (crisp)

> **The beat never reaches a color channel directly. Luminance always changes
> smoothly.** Route a beat through a *smoothed var* and let that var drive
> **geometry** (radius, size, length, position) — felt as a punch, not seen as a
> strobe.

A raw `beat` snaps from 0 to 1.0 in a single frame. Wired to a brightness or color
channel, that is a full-canvas white flash on every onset — a strobe. The project
bans it.

### The problem

You want the beat to be *felt*. The wrong way is the obvious way: `"v": "beat"` or
`"a": "beat"`. The right way routes the beat's energy into motion.

### The pattern

A smoothed **kick** var, then drive geometry with it. The runnable example
([`recipe-flash-free-beats.artifact.json`](./examples/recipe-flash-free-beats.artifact.json)):

```json
"vars": { "kick": { "init": "0", "frame": "max(kick * 0.9, beat * 0.5)" } }
```

- `beat * 0.5` — the onset, scaled down so it never dominates.
- `kick * 0.9` — the previous kick, decayed ~10% per frame.
- `max(...)` — take whichever is larger: a beat snaps `kick` up, then it eases back
  down between beats. This is a smooth envelope, not a 1-frame spike.

Then `kick` drives **geometry only** — the ring radius and dot size punch outward:

```json
"x": "(settings.radius + settings.punch * kick) * cos(u * tau + spin) / aspect",
"w": "(0.018 + 0.02 * band(u) + 0.02 * kick) / aspect"
```

Color brightness, meanwhile, stays gated on **energy** through `smoothstep`, so it
changes smoothly and is independent of the raw onset:

```json
"r": "settings.tint_r * (0.1 + 0.9 * smoothstep(0.02, 0.12, energy) * (0.3 + 0.7 * band(u)))"
```

### Forbidden, by contrast

Do **not** write any of these — each is a strobe:

```json
"v": "beat"                         // full-canvas luminance flash every onset
"a": "0.2 + 0.8 * beat"             // alpha strobe
"r": "beat", "g": "beat", "b": "beat"   // white flash
```

The contrast is the whole point: beat → smoothed var → **geometry**; luminance →
**smoothstep on energy**.

### Variations

- **Different envelopes.** `max(kick * 0.85, beat)` decays faster; a symmetric
  smoother (`kick * 0.9 + beat * 0.1`) gives an even gentler swell.
- **Kick the warp/spin.** Add `+ 0.9 * kick` inside an accumulating speed var for a
  beat-driven lurch (Starfield Warp's warp speed does this).
- **Bloom the length.** Kaleido Petals adds the smoothed beat to *petal length only*,
  never to color — a textbook flash-free bloom.

### Pitfalls

- **Any color channel = `beat`.** The cardinal sin. Always interpose a smoothed var
  *and* route it to geometry, not color.
- **Driving color with the kick var.** Even smoothed, pushing `kick` into a brightness
  channel re-introduces a (softer) flash. Keep luminance on `energy`; keep the beat on
  geometry.
- **Too strong a kick.** A huge `punch` makes the whole scene jump distractingly. A
  little goes a long way.

---

<sub>Prev: [3. Scenes, Layers & Settings](./03-scenes-layers-settings.md) ·
Next: [5. Gallery](./05-gallery.md) ·
[Table of contents](./README.md)</sub>
