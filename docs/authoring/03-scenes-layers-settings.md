# 3. Scenes, Layers & Settings

> Purpose: the structural reference — the artifact skeleton, the four setting types, state variables, the three layer types field by field, color models, feedback, the limits, and contract versioning. Each piece links to a focused runnable example.

Chapter 2 taught the language inside the values. This chapter is the structure *around* them: the JSON shape of an artifact and what every key does. It is a reference — skim the skeleton, then jump to the section you need. For the last word on any rule, the [artifact contract](../reference/artifact-contract.md) is normative; this chapter teaches and links to it.

---

## 3.1 The artifact skeleton

Every artifact is one UTF-8 JSON file named `<something>.artifact.json` with this shape:

```json
{
  "contract": "1.0",
  "meta": { "id": "my-artifact", "name": "My Artifact", "description": "…", "author": "…" },
  "settings": { },
  "vars": { },
  "feedback": { "decay": "0.92" },
  "scene": [ ]
}
```

| Key | Required | What it is |
|---|---|---|
| `contract` | yes | Contract version, `"MAJOR.MINOR"` (see [§3.10](#310-contract-versioning)). For v1.0 always `"1.0"`. |
| `meta` | yes | Identity. `id` is unique, `[a-z][a-z0-9-]*`, ≤ 64 chars; `name` is the dropdown label; `description` and `author` are optional. |
| `settings` | no | User-adjustable controls, auto-rendered in the top-right panel; up to 32 ([§3.2](#32-settings)). |
| `vars` | no | Named per-frame state with `init`/`frame` formulas; up to 64 ([§3.3](#33-state-variables-vars)). |
| `feedback` | no | Scene-level trails ([§3.7](#37-feedback-trails)). |
| `scene` | yes | 1–16 layers, drawn in order — **later layers draw on top** ([§3.4](#34-layers-the-scene)). |

The minimal valid artifact is one layer with no settings, vars, or feedback:

```json
{
  "contract": "1.0",
  "meta": { "id": "minimal", "name": "Minimal" },
  "scene": [
    { "type": "field", "resolution": 8,
      "cell": { "color": { "model": "rgba", "r": "0.1", "g": "0", "b": "0.2", "a": "1" } } }
  ]
}
```

Reference: [contract §1](../reference/artifact-contract.md#1-top-level-shape).

---

## 3.2 Settings

A setting is a control the app draws automatically in the top-right panel; the user adjusts it live and your formulas read the result within one frame. Each entry is `"name": { "type": …, "label": …, "default": … }`. Names follow `[a-z][a-zA-Z0-9_]*`, must be unique, and must not shadow a built-in identifier (the §2.4 input names are reserved).

> **Reading rule (from chapter 2):** settings are read with the **`settings.` prefix** — `settings.bars`, never bare `bars`. Only vars are read bare.

There are four types. Here is what the panel renders and how each appears in a formula:

| `type` | Panel widget | In a formula | Fields |
|---|---|---|---|
| `number` | slider | `settings.name` = the current value | `min`, `max`, optional `step`, `default` |
| `toggle` | checkbox | `0.0` (off) or `1.0` (on) | `default`: `true`/`false` |
| `choice` | dropdown | **0-based index** of the selected option | `options`: 2–16 strings, `default` ∈ options |
| `color` | color picker | expands to `settings.name_r/_g/_b/_a`, each 0..1 | `default`: `"#RRGGBB"` or `"#RRGGBBAA"` |

### number

```json
"count": { "type": "number", "label": "Dots", "min": 3, "max": 64, "step": 1, "default": 18 }
```

Read as `settings.count`. Use it for counts, sizes, speeds, smoothing amounts. Runnable: [`ref-settings-number.artifact.json`](./examples/ref-settings-number.artifact.json).

### toggle

```json
"glow": { "type": "toggle", "label": "Glow", "default": true }
```

Reads as `0.0` or `1.0`, so it slots straight into `if(settings.glow, …, …)` or a multiplier. Runnable: [`ref-settings-toggle.artifact.json`](./examples/ref-settings-toggle.artifact.json).

### choice

```json
"palette": { "type": "choice", "label": "Palette", "options": ["warm", "cool", "mono"], "default": "cool" }
```

> **Choice is a 0-based index.** With `options: ["warm", "cool", "mono"]`, `settings.palette == 0` tests for `"warm"`, `== 1` for `"cool"`, `== 2` for `"mono"`. Select a per-option value by summing masked comparisons: `20 * (settings.palette == 0) + 200 * (settings.palette == 1)`. Runnable: [`ref-settings-choice.artifact.json`](./examples/ref-settings-choice.artifact.json).

### color

```json
"tint": { "type": "color", "label": "Tint", "default": "#33ccff" }
```

> **A color setting expands into four channel inputs**, each 0..1: `settings.tint_r`, `settings.tint_g`, `settings.tint_b`, `settings.tint_a`. There is no `settings.tint` you can read as a whole — you read the channels. (`#RRGGBB` defaults give `_a = 1`; use `#RRGGBBAA` to set a default alpha.) Feed them into an `rgba` color, or scale them by an energy gate for calm silence. Runnable: [`ref-settings-color.artifact.json`](./examples/ref-settings-color.artifact.json).

The panel updates apply within one frame, and "Reset to defaults" restores every `default`. Normative: [contract §4](../reference/artifact-contract.md#4-settings-settings).

---

## 3.3 State variables (`vars`)

Settings are adjusted by the user; **vars** are computed by you, once per frame, to carry state across frames — smoothers, accumulators, latches, peak-holds. Each var has two formulas:

```json
"vars": {
  "bass_smooth": { "init": "0", "frame": "bass_smooth * 0.85 + low * 0.15" },
  "spin":        { "init": "0", "frame": "spin + dt * (0.2 + energy)" }
}
```

- **`init`** runs once when the artifact becomes active (and on "Reset to defaults").
- **`frame`** runs once per rendered frame, **in declaration order** (top to bottom).

### Read semantics — the one rule that matters

Inside a `frame` formula:

- a var declared **earlier this frame** reads its **already-updated** (this-frame) value;
- a var reading **itself, or a var declared later**, reads the **previous frame's** value.

That single rule makes smoothing, decay, and accumulation natural. Element/point/cell formulas read every var at its current-frame value and cannot write to vars.

#### Worked pair: a smoother and an accumulator

```json
"vars": {
  "low_s": { "init": "0", "frame": "low_s * 0.85 + low * 0.15" },
  "bar":   { "init": "0", "frame": "low_s" }
}
```

- **`low_s` is a one-pole smoother.** Its `frame` reads `low_s` — *itself* — so that read is last frame's smoothed value, blended 85/15 with this frame's raw `low`. The result is a gently lagging follower of the bass: it rises and falls smoothly instead of jittering. This is the **flash-free** building block — beats reach geometry only after being rounded off by a smoother.
- **`bar` is a downstream reader.** It is declared *after* `low_s`, so `bar = low_s` sees **this frame's** freshly-updated smoothed value (not last frame's). Swap the declaration order and `bar` would lag one frame behind. That is the declaration-order rule in action.

A pure accumulator looks the same shape: `"spin": { "init": "0", "frame": "spin + dt * (0.2 + energy)" }` reads itself (last frame's angle) and adds this frame's increment — a clock that speeds up with loudness and, crucially, *stops advancing its energy term* in silence.

Runnable: [`ref-vars-smoothing.artifact.json`](./examples/ref-vars-smoothing.artifact.json). For asymmetric "fast attack, slow release" peak-holds (`if(low > low_g, low, low_g * decay)`) see the [peak-hold recipe](./04-recipes.md). Normative: [contract §5](../reference/artifact-contract.md#5-state-variables-vars).

---

## 3.4 Layers (the scene)

`scene` is an array of 1–16 layers, drawn front-to-back in array order — **the last layer is on top**. Put a dim background field first, glowing detail last.

Three keys are common to every layer:

| Key | Default | Meaning |
|---|---|---|
| `type` | — (required) | `"instanced"`, `"polyline"`, or `"field"` |
| `blend` | `"alpha"` | `"alpha"` = normal over; `"add"` = additive (glow, light accumulation) — see [§3.6](#36-blend-modes) |
| `visible` | `1` | Formula; `< 0.5` skips the whole layer this frame (the correct way to hide a layer) |

The three layer types are next, each with a fragment and a link to its focused example.

---

### 3.4.1 `instanced` — N shapes

Draws `count` copies of one shape (bars, particles, radial patterns). Each copy is positioned and colored by `element` formulas that see `i`, `n`, `u`.

```json
{
  "type": "instanced", "shape": "rect", "blend": "add",
  "count": "settings.bars",
  "element": {
    "x": "-1 + (2 * i + 1) / n", "y": "-1 + band(u)",
    "w": "1.6 / n", "h": "2 * band(u)", "rot": "0",
    "color": { "model": "hsva", "h": "u * 360", "s": "1", "v": "0.4 + 0.6 * band(u)", "a": "1" }
  }
}
```

| Field | Type | Notes |
|---|---|---|
| `shape` | `"rect"` / `"circle"` / `"triangle"` | all **filled**; for a ring *outline* use a closed `polyline` ([§3.4.2](#342-polyline--connected-strip)) |
| `count` | Formula | floored to an integer, then clamped **1..4096** |
| `element.x`, `.y` | Formula | center position in −1..1 |
| `element.w`, `.h` | Formula | width / height in coordinate units |
| `element.rot` | Formula | rotation in **radians**, counter-clockwise about the center; default `0` |
| `element.color` | Color | per-instance ([§3.5](#35-color)) |

> **`count` never reaches 0.** The clamp floor is 1, so `count: settings.some_toggle` still draws one instance even when the toggle is off. To make a layer disappear, gate it with `visible` (`< 0.5` skips it), not with `count`.

> **Elements are positional, not persistent.** Each frame, element `i` is recomputed from scratch — there is no stored velocity or lifetime. If `count` changes, `u = i/max(n−1,1)` shifts for every element, so prefer a constant or settings-driven `count` for stable layouts. Particles are therefore authored as *closed-form trajectories* `f(rand(i), age)`, not simulations — see the [particles recipe](./04-recipes.md).

Runnable: [`ref-instanced.artifact.json`](./examples/ref-instanced.artifact.json) (a grid using `i`/`n` index math). Normative: [contract §6.1](../reference/artifact-contract.md#61-instanced--n-shapes-bars-particles-radial-patterns).

---

### 3.4.2 `polyline` — connected strip

Draws a continuous line through `points` vertices (oscilloscopes, curves, ring outlines). Each point sees `i`, `n`, `u`; color is interpolated along the strip.

```json
{
  "type": "polyline", "points": 256, "thickness": "2 + 6 * energy", "closed": false,
  "point": {
    "x": "-1 + 2 * u", "y": "wave(u) * 0.8",
    "color": { "model": "rgba", "r": "0.2", "g": "1", "b": "0.6", "a": "1" }
  }
}
```

| Field | Type | Notes |
|---|---|---|
| `points` | integer | clamped **2..4096** |
| `thickness` | Formula | logical pixels, clamped **0.1..64** |
| `closed` | bool | `true` joins last→first (radial scopes, rings); default `false` |
| `point.x`, `.y` | Formula | vertex position; `u` walks 0→1 along the line |
| `point.color` | Color | per-point, interpolated between vertices |

The **ring idiom** lives here: a filled `circle` shape is a disc, so for a *ring outline* lay a closed polyline on a circle — `x = r * cos(u * tau) / aspect`, `y = r * sin(u * tau)` (note the `/ aspect` for a true circle), with `r` gated on audio for a pulse. See the [true-circles recipe](./04-recipes.md).

Runnable: [`ref-polyline.artifact.json`](./examples/ref-polyline.artifact.json) (a waveform trace). Normative: [contract §6.2](../reference/artifact-contract.md#62-polyline--connected-strip-oscilloscope-curves).

---

### 3.4.3 `field` — full-canvas color grid

Evaluates one color per cell of a `resolution × resolution` grid (plasmas, gradients, backgrounds). Each cell sees its center `x`, `y` (and `i`, `n`).

```json
{
  "type": "field", "resolution": 48,
  "cell": { "color": { "model": "hsva",
    "h": "t * 20 + 180 * sin(x * 2 + t) * sin(y * 2 - t)",
    "s": "0.8", "v": "energy * (0.5 + 0.5 * sin(x * y * 4 + t * 2))", "a": "1" } }
}
```

| Field | Type | Notes |
|---|---|---|
| `resolution` | integer | cells per axis, floored, clamped **2..128** (≤ 16 384 cells). Cost is resolution² — keep it modest for backgrounds. |
| `cell.color` | Color | evaluated at the cell center `x`, `y` (−1..1, y up) |

Prefer `x`/`y` for spatial effects — they are layout-independent (cells iterate row-major from bottom-left as `i = row * resolution + col`, but you rarely need `i`).

Runnable: [`ref-field.artifact.json`](./examples/ref-field.artifact.json) (an energy-gated gradient). Normative: [contract §6.3](../reference/artifact-contract.md#63-field--full-canvas-color-grid-plasma-gradients).

---

## 3.5 Color

Every color is `{ "model": "rgba" | "hsva", channels as Formulas, "a" optional (default 1) }`. Any channel may reference audio, time, or vars — dynamic color is first-class.

| Model | Channels | Ranges |
|---|---|---|
| `rgba` | `r`, `g`, `b`, optional `a` | each clamped 0..1 |
| `hsva` | `h`, `s`, `v`, optional `a` | `h` in degrees (wrapped mod 360); `s`, `v`, `a` clamped 0..1 |

**When to use which:**

- **`hsva`** for anything where you want to *rotate hue* or *vary brightness independently* — rainbows (`h: "u * 360"`), spectrum coloring (`h` from `band`), and the calm-silence gate on `v` alone (`v: "0.1 + 0.7 * smoothstep(0.02, 0.2, energy)"`). This is the everyday choice in this manual.
- **`rgba`** when you set channels directly or feed a color setting straight through (`r: "settings.tint_r * gate"`), or when blending two explicit colors.

Both models, side by side, in [`ref-colors.artifact.json`](./examples/ref-colors.artifact.json). Normative: [contract §7](../reference/artifact-contract.md#7-color).

---

## 3.6 Blend modes

Per layer, `"blend"` is `"alpha"` (default) or `"add"`.

- **`alpha`** — normal "paint over": the layer's alpha controls how much it covers what's beneath. Use for opaque backgrounds and solid shapes.
- **`add`** — additive: the layer's color is *added* to what's beneath, so overlaps brighten toward white. Use for glow, light, sparks, and anything you want to bloom. Most glowing built-ins draw their bright layers with `add`. Because adds accumulate brightness, keep their `v`/channels energy-gated so silence does not stay lit.

Each layer chooses independently — a common pattern is an `alpha` background field plus `add` detail layers on top.

---

## 3.7 Feedback (trails)

```json
"feedback": { "decay": "0.92" }
```

Feedback gives the whole scene **trails**: before any layer draws, the previous frame's image is multiplied by `decay` (a Formula, clamped 0..0.99). High decay → long persistent trails; low decay → a short smear; absent → no trails.

**What it costs:** one extra offscreen pass per frame. It is the **first feature shed** when the app's performance-degradation ladder kicks in — so treat trails as enhancement, not as load-bearing structure: the artifact must still read correctly without them. Gate the decay or the trailing layers' brightness by energy so a silent scene fades to calm instead of holding a frozen smear.

Runnable: [`ref-feedback.artifact.json`](./examples/ref-feedback.artifact.json) (an orbiting dot leaving a trail). The full trails idiom is in the [trails recipe](./04-recipes.md). Normative: [contract §8](../reference/artifact-contract.md#8-feedback-trails).

---

## 3.8 Determinism

An artifact's output is a pure function of the audio feature frames, `t`/`dt` (both from the audio sample clock), the setting values, and the render aspect. `rand(k)` is a stateless hash of (artifact id, `k`) — no hidden state anywhere. Same file + same inputs + same app build ⇒ identical output. Normative: [contract §9](../reference/artifact-contract.md#9-determinism--purity-guarantees).

---

## 3.9 Limits summary

All enforced at load — a file that exceeds any limit is rejected (logged, not crashed). Every example in this manual stays comfortably under these with margin.

| Limit | Value |
|---|---|
| Layers per scene | 16 |
| Instances per `instanced` / points per `polyline` | 4096 |
| `field` resolution | 128 (⇒ ≤ 16 384 cells) |
| Settings / vars | 32 / 64 |
| Formula string length | 1024 characters |
| Ops per formula / nesting / ops per artifact | 256 / 32 / 16 384 |
| File size | 256 KiB |

Full table: [contract §12](../reference/artifact-contract.md#12-limits-summary-load-time-enforced).

---

## 3.10 Contract versioning

The `contract` field is `"MAJOR.MINOR"`. A loader supports one MAJOR and a highest MINOR within it, and accepts a file when `MAJOR == 1` **and** the file's MINOR ≤ the loader's supported MINOR (so a 1.2 app loads 1.0/1.1/1.2 files, but a 1.0-only app rejects a 1.1 file). Within an accepted version, unknown keys are rejected — strict mode keeps authoring typos loud (`"colour"` won't silently no-op).

For everything authored against this manual, write `"contract": "1.0"`. The full versioning and evolution policy is normative in [contract §10](../reference/artifact-contract.md#10-versioning--evolution).

---

Prev: [2. The Formula Language](./02-formula-language.md) · Next: [4. Recipes](./04-recipes.md) · [Table of contents](./README.md)
