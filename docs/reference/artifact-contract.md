# ViewMusic Artifact Contract — v1.0

The single published contract every visual artifact conforms to — built-in and user-created
alike. Machine-readable schema: [`artifact.schema.json`](./artifact.schema.json)
(JSON Schema draft 2020-12, generated from the implementation's types). Reference
example: [`spectrum-bars.artifact.json`](../../assets/builtin-artifacts/spectrum-bars.artifact.json).

Learning-oriented manual with tutorial and recipes: [`../authoring/README.md`](../authoring/README.md).

An artifact is one UTF-8 JSON file with extension `.artifact.json`, placed in
`~/Library/Application Support/io.github.acrive82.viewmusic/artifacts/` (user) or shipped
embedded (built-ins). Validation errors are reported in the app log file with the file name
and JSON path.

## 1. Top-level shape

```json
{
  "contract": "1.0",
  "meta": { "id": "my-artifact", "name": "My Artifact", "description": "…", "author": "…" },
  "settings": { … },
  "vars": { … },
  "scene": [ … ],
  "feedback": { "decay": "0.92" }
}
```

| Key | Required | Description |
|---|---|---|
| `contract` | yes | Contract version, `"MAJOR.MINOR"`. Accepted iff MAJOR is supported and MINOR ≤ the loader's supported minor (see §10); otherwise rejected |
| `meta` | yes | `id` (unique, `[a-z][a-z0-9-]*`, ≤64 chars), `name` (dropdown label), optional `description`, `author` |
| `settings` | no | User-adjustable controls, auto-rendered in the top-right panel (≤32) |
| `vars` | no | Named per-frame state variables with `init`/`frame` formulas (≤64) |
| `scene` | yes | 1–16 layers, drawn in order (later layers on top) |
| `feedback` | no | Scene-level trails: previous frame multiplied by `decay` each frame |

## 2. Formulas

Anywhere the schema says **Formula**, write either a JSON number (constant) or a string
containing a pure mathematical expression. Formulas are compiled once at load; they cannot
loop, recurse, define functions, or touch anything outside the documented inputs.
Limits: ≤256 compiled operations per formula, nesting ≤32, ≤16 384 total
operations per artifact.

### 2.1 Operators

`+ - * / %` (remainder), `^` (power), unary `-`, comparisons `< <= > >= == !=` (yield
0 or 1), logical `&& || !` (operands treated as ≥0.5 ⇒ true; yield 0 or 1), parentheses.

### 2.2 Functions

| Group | Functions |
|---|---|
| Trig | `sin(x)`, `cos(x)`, `tan(x)`, `asin(x)`, `acos(x)`, `atan(x)`, `atan2(y,x)` — radians |
| Exp/log | `exp(x)`, `log(x)` (natural), `log2(x)`, `log10(x)`, `pow(x,y)`, `sqrt(x)` |
| Shaping | `abs(x)`, `sign(x)`, `floor(x)`, `ceil(x)`, `round(x)`, `fract(x)`, `min(a,b)`, `max(a,b)`, `clamp(x,lo,hi)`, `mix(a,b,k)` (lerp), `smoothstep(e0,e1,x)`, `step(edge,x)` |
| Conditional | `if(cond, then, else)` — true branch when `cond ≥ 0.5`; both branches are evaluated (pure, no side effects), so guard divisions: `if(d != 0, n/d, 0)` |
| Audio | `band(u)`, `wave(u)` — see §3 |
| Random | `rand(k)` — deterministic stateless hash of (artifact seed, `k`); uniform 0..1. **Same `k` ⇒ same value on every frame** — random values are stable unless you change `k`. Per-element stable randomness: fold the index into `k` (`rand(i)`, `rand(i + 100)` for a second independent value). Re-rolling over time is opt-in: fold a time-derived term into `k`, e.g. `rand(i + beat_count * 1000)` re-rolls each particle's value on every beat |
| Constants | `pi`, `tau`, `e` |

Any operation producing NaN or ±Infinity is replaced with 0 at that step; final
geometry/color values are clamped to their documented ranges. A faulty formula therefore
degrades visibly but can never crash the app.

### 2.3 Inputs (read-only identifiers)

Available in **all** stages:

| Name | Range | Meaning |
|---|---|---|
| `t` | seconds | Audio-derived clock (monotonic; from sample counter, not wall clock) |
| `dt` | seconds | Delta of `t` between consecutive rendered frames (nominally 1/60 s at the app's fixed 60 Hz pacing; audio-derived, never wall clock) |
| `energy` | 0..1 | Smoothed overall loudness |
| `low`, `mid`, `high` | 0..1 | Band aggregates (<250 Hz / 250 Hz–4 kHz / >4 kHz) |
| `beat` | 0..1 | 1.0 on onset, exponential decay after |
| `beat_count` | integer | Total onsets since artifact start |
| `band(u)` | 0..1 | Spectrum lookup at normalized position u∈0..1 across 48 log-spaced bands (interpolated). Exact band k (0..47): `band(k/47)` |
| `wave(u)` | −1..1 | Waveform lookup at u∈0..1 across the current 256-sample window (interpolated). Exact sample k (0..255): `wave(k/255)` |
| `aspect` | >0 | Render width / height |
| `settings.*` | varies | Each declared setting, read with the **`settings.` prefix** — `settings.name` (see §4 value mapping). A bare `name` is *not* the setting; only vars are read bare (next row) |
| *(var names)* | varies | Declared `vars` by **bare name** — `name`, no prefix (see §5 read semantics) |

Additional inputs per **stage**:

| Stage | Extra inputs |
|---|---|
| `instanced.element` | `i` (element index, 0…n−1), `n` (instance count this frame), `u` = `i/max(n−1,1)` ∈ 0..1 |
| `polyline.point` | `i`, `n`, `u` (position along the line) |
| `field.cell` | `x`, `y` (cell-center coordinates, −1..1; y up), plus `i`, `n` (cell index/count) |

**Coordinate system**: x ∈ −1..1 left→right, y ∈ −1..1 bottom→top, independent of window
size; use `aspect` (= width/height) to correct for non-square windows. Sizes (`w`, `h`) are
in the same units. **Circle idiom** — coordinates stretch with the window, so a naive
`(cos(a), sin(a))` draws an ellipse; for a true circle divide every x-distance by `aspect`:
`x = cx + r * cos(a) / aspect`, `y = cy + r * sin(a)` — and likewise `w = d / aspect`,
`h = d` to keep an element visually square.

**Note on `band(u)`/`wave(u)` arguments**: their `u` is *any* expression in 0..1 you choose —
it does not have to be the element's `u`. `band(0.5)` samples mid-spectrum from any stage;
`band(u)` maps element position onto the spectrum.

## 3. Audio semantics

Features refresh ~94×/s from the live system mix; the renderer always reads the newest frame.
During silence the app feeds **zeroed features** (`energy`, `low/mid/high`, `band()`,
`wave()`, `beat` all → 0) — but **`t` and `dt` keep advancing** (the audio clock never
stops). Write artifacts so zero input produces a calm idle look: gate motion and
brightness by the audio, not just by time. Idiom — multiply velocities, sizes, or alpha by
`energy` (or `smoothstep(0.02, 0.1, energy)` for a clean fade): when sound stops, the scene
settles instead of continuing to animate on `t` alone. There is no `silence` input; sustained
zero energy *is* the silence signal.

## 4. Settings (`settings`)

Each entry: `"name": { "type": …, "label": …, …, "default": … }`. Name rules:
`[a-z][a-zA-Z0-9_]*`, unique, not shadowing built-in identifiers (§2.3 names are reserved).

| `type` | Fields | In formulas |
|---|---|---|
| `number` | `min`, `max`, optional `step`, `default` | `settings.name` = current value |
| `toggle` | `default`: true/false | 0.0 or 1.0 |
| `choice` | `options`: [2–16 strings], `default` ∈ options | 0-based index of the selected option (= its position in `options`). Example: `"options": ["thin", "wide"]` → `settings.style == 0` tests for `"thin"`, `settings.style == 1` for `"wide"` |
| `color` | `default`: `"#RRGGBB"` or `"#RRGGBBAA"` | `settings.name_r/_g/_b/_a` ∈ 0..1 |

The top-right panel is generated from these declarations: number→slider,
toggle→checkbox, choice→dropdown, color→color picker. Changes apply within one frame;
"Reset to defaults" restores every `default`.

## 5. State variables (`vars`)

```json
"vars": {
  "bass_smooth": { "init": "0", "frame": "bass_smooth * 0.85 + low * 0.15" },
  "spin":        { "init": "0", "frame": "spin + dt * (0.2 + energy)" }
}
```

- `init` runs once when the artifact becomes active (and on settings reset).
- `frame` runs once per rendered frame, **in declaration order**.
- **Read semantics**: in a `frame` formula, a var declared *earlier this frame* reads its
  already-updated current value; reading *itself or a later var* reads the previous frame's
  value. This makes smoothing/decay/accumulation natural and is fully deterministic.
- Element/point/cell formulas read all vars at their current-frame values; they cannot write.

## 6. Layers (`scene`)

Common to all layers: `"blend": "alpha" | "add"` (default `"alpha"`; `"add"` for glow),
optional `"visible"`: Formula (default 1; < 0.5 skips the layer this frame).

### 6.1 `instanced` — N shapes (bars, particles, radial patterns)

```json
{
  "type": "instanced", "shape": "rect", "blend": "add",
  "count": "settings.bars",
  "element": {
    "x": "-1 + 2*u + 1/n", "y": "-1 + band(u)", "w": "1.6/n", "h": "2*band(u)",
    "rot": "0",
    "color": { "model": "hsva", "h": "u*360", "s": "1", "v": "0.4 + 0.6*band(u)", "a": "1" }
  }
}
```

`shape`: `"rect" | "circle" | "triangle"` (all **filled**; for a ring *outline* use a
closed `polyline`, §6.2). `count`: per-frame Formula — **floored** to an integer, then
clamped 1..4096. The clamp floor is 1, so `count` can never reach 0: a `count` that
evaluates to 0 (e.g. `count: settings.some_toggle`) still draws one instance. To make a
layer disappear, gate it with `visible` (< 0.5 skips the whole layer, §6), not with
`count`. `element` formulas run per instance with `i`, `u`, `n`. `rot`: rotation in
**radians**, counter-clockwise, about the element's center; default 0 (e.g. radial bars
pointing outward: `rot = u * tau`).

**Element identity is positional, not persistent.** Elements carry no state between frames:
each frame, element `i` is recomputed from scratch, and if `count` changes, `u = i/max(n−1,1)`
shifts for every element. Prefer a constant or settings-driven `count` for stable layouts.
**Particle idiom** (bursts, sparks): treat each element as a *closed-form trajectory* —
fixed per-element randomness from `rand(i + c)`, launch time from a var (e.g.
`lastBeatT: { "init": "0", "frame": "if(beat >= 1, t, lastBeatT)" }`), then position =
`f(rand(i), t - lastBeatT)`. Re-roll directions per burst with `rand(i + beat_count * 1000)`.
Per-element physics with persistent velocity/lifetime is intentionally not expressible
(purity/boundedness, §9) — design particles as functions of (index, age), not as simulations.

### 6.2 `polyline` — connected strip (oscilloscope, curves)

```json
{
  "type": "polyline", "points": 256, "thickness": "2 + 6*energy", "closed": false,
  "point": {
    "x": "-1 + 2*u", "y": "wave(u) * 0.8",
    "color": { "model": "rgba", "r": "0.2", "g": "1", "b": "0.6", "a": "1" }
  }
}
```

`points` clamped 2..4096; `thickness` in logical pixels, clamped 0.1..64; per-point color
interpolated along the strip; `closed: true` joins last→first (radial scopes).

**Ring idiom** — a closed polyline laid on a circle is the way to draw a *ring outline*
(instanced `circle` shapes are filled discs, not rings): place each point at angle
`u * tau` and gate the radius on audio for a pulsing ring, applying the circle aspect
correction (§2.3): `x = r * cos(u * tau) / aspect`, `y = r * sin(u * tau)`, with e.g.
`r = 0.5 + 0.3*low`. Color each segment by spectrum with `band(u)`. For concentric rings,
stack several such layers with different radii/bands. Note: with `closed: true` the point
at `u = 1` (angle `tau`) coincides with the point at `u = 0`, so the closing segment is
zero-length and harmless; if you prefer to avoid the doubled point, span the angle with
`u * tau * (n-1)/n` instead.

### 6.3 `field` — full-canvas color grid (plasma, gradients)

```json
{
  "type": "field", "resolution": 48,
  "cell": { "color": { "model": "hsva",
    "h": "t*20 + 180*sin(x*2 + t) * sin(y*2 - t)",
    "s": "0.8", "v": "energy * (0.5 + 0.5*sin(x*y*4 + t*2))", "a": "1" } }
}
```

`resolution` = cells per axis (floored, clamped 2..128; evaluation cost is resolution²,
bounded). Cell color evaluated at the cell center (`x`, `y`). Cells iterate in
row-major order from bottom-left (`i = row * resolution + col`); prefer `x`,`y` for spatial
effects — they are layout-independent.

## 7. Color

`{ "model": "rgba" | "hsva", channels as Formulas, "a" optional (default 1) }`.
`rgba`: r,g,b,a clamped 0..1. `hsva`: `h` in degrees (wrapped mod 360), s,v,a clamped 0..1.
Dynamic color is first-class — any channel may reference audio/time/vars.

## 8. Feedback (trails)

`"feedback": { "decay": Formula }` (clamped 0..0.99): each frame, the previous frame's image
is multiplied by `decay` before layers draw — classic trails/persistence. Costs one
offscreen pass; it is the first feature shed by the performance degradation ladder.

## 9. Determinism & purity guarantees

- An artifact's output is a pure function of: the ordered audio feature frames, `t`/`dt`
  (both derived from the audio sample clock — under test, the harness supplies the whole
  (t, dt, features) sequence), setting values, and render aspect. `rand(k)` is a
  stateless hash of (artifact-id seed, `k`) — no hidden state anywhere.
- No file, network, system, or inter-artifact access exists in the language.
- Same file + same inputs + same app build ⇒ identical output (the basis for the
  synthetic-stream regression tests in the test suite).

## 10. Versioning & evolution

`contract` uses `MAJOR.MINOR`. A loader supports one MAJOR and a highest MINOR within it,
and accepts a file iff `MAJOR == 1` and the file's MINOR ≤ the loader's supported MINOR
(e.g. a 1.2-capable app loads 1.0, 1.1, 1.2 files; a 1.0-only app rejects a 1.1 file).
Within an accepted version, unknown keys are rejected (strict mode keeps authoring errors
loud — `"colour"` won't silently no-op; this is why newer-MINOR files are rejected rather
than half-understood). MINOR bumps add optional capabilities; MAJOR bumps may break shape.
The version check runs before schema validation, so out-of-range files log a clear
`unsupported contract version X.Y`, not a schema pattern error.

## 11. Diagnostics

All artifact problems go to the log file only:
`~/Library/Logs/io.github.acrive82.viewmusic/viewmusic.log`. Entries name the file, the
validation step, the JSON path (e.g. `scene[0].element.color.h`), and the offending token.
The UI never shows artifact errors; a rejected file simply does
not appear in the dropdown.

## 12. Limits summary (load-time enforced)

| Limit | Value |
|---|---|
| Layers per scene | 16 |
| Instances per `instanced` layer / points per `polyline` | 4096 |
| `field` resolution | 128 (⇒ ≤16 384 cells) |
| Settings / vars | 32 / 64 |
| Formula string length | 1024 characters |
| Ops per formula / nesting / ops per artifact | 256 / 32 / 16 384 |
| File size | 256 KiB |
