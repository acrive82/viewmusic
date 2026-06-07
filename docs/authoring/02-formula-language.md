# 2. The Formula Language

> Purpose: teach the small expression language that drives every number in an artifact — from "what is a formula" to a complete reference of every operator, function, constant, and input, with stage availability.

Everything that can move, glow, or change in an artifact is a **formula**. A bar's height, a particle's color, a layer's visibility, a smoothing variable's update — all of them are formulas. Learn this one language and you can author anything the contract allows.

This chapter is both a tutorial (read it top to bottom once) and a reference (jump back to a table later). For the normative, last-word definition of any rule here, see the [artifact contract §2](../reference/artifact-contract.md#2-formulas); this chapter teaches and exemplifies it.

---

## 2.0 What a formula is

Anywhere a layer field expects a value, you may write **either**:

- a **JSON number** — a literal constant, e.g. `0.5`, `64`, `-1`; or
- a **JSON string** containing a mathematical expression, e.g. `"0.5 + 0.5 * sin(t)"`.

That is the whole rule. The two are interchangeable for constants — `"w": 0.2` and `"w": "0.2"` mean the same thing. You only *need* a string when the value is an expression (it references an input, calls a function, or uses an operator).

```json
"w": 0.2,                      ← number: a fixed width
"h": "2 * band(u)",           ← string: height tracks the spectrum
"rot": "u * tau"              ← string: rotation around the circle
```

A formula is **pure**: it reads the documented inputs (time, audio, your settings and vars, the per-element index) and returns one number. It cannot loop, recurse, define functions, branch with side effects, or touch anything outside those inputs. It is compiled once when the artifact loads and then evaluated many times per frame — so it is fast, and any mistake is caught at load time, not mid-song.

**Limits** (enforced at load — you will never hit them with sensible formulas): ≤ 256 operations per formula, nesting ≤ 32, ≤ 1024 characters per string, ≤ 16 384 operations across the whole artifact. See [§12 of the contract](../reference/artifact-contract.md#12-limits-summary-load-time-enforced) and the [limits summary in chapter 3](./03-scenes-layers-settings.md#39-limits-summary).

---

## 2.1 Operators

The language has the operators you would expect from a calculator, plus comparisons and logicals that yield `0` or `1`.

| Operator | Meaning | Example expression | Result |
|---|---|---|---|
| `+` `-` `*` `/` | add, subtract, multiply, divide | `2 + 3 * 4` | `14` |
| `%` | remainder | `7 % 3` | `1` |
| `^` | power (right-associative) | `2 ^ 3 ^ 2` | `512` (= `2^(3^2)`) |
| unary `-` | negation | `-energy` | negated |
| `<` `<=` `>` `>=` `==` `!=` | comparison → `0` or `1` | `low > 0.5` | `1` when true |
| `&&` `\|\|` `!` | logical and / or / not → `0` or `1` | `low > 0.3 && high > 0.3` | `1` when both |
| `( )` | grouping | `(1 + 2) * 3` | `9` |

**Comparisons and logicals yield numbers.** `low > 0.5` is `1.0` when true and `0.0` when false — which is exactly what you want for a gate: `0.2 + 0.8 * (energy > 0.4)` jumps brightness when energy crosses the threshold. Logical operands are *truthy* when `>= 0.5`, so `!silence_flag` and `gate_a && gate_b` work on any number, not just exact `0`/`1`.

**Precedence** (lowest binds loosest, highest binds tightest):

```
||  <  &&  <  comparisons  <  + -  <  * / %  <  unary - / !  <  ^  <  function call / value
```

When in doubt, parenthesize — it never hurts and it documents intent.

> ⚠️ **Unary minus binds *looser* than power.** `-2^2` parses as `-(2^2) = -4`, **not** `(-2)^2 = 4`. This is deliberate and matches every reference. If you want the squared negative, write `(-2)^2`. (Power is also right-associative: `2^3^2 = 2^(3^2) = 512`.)

The grouped operator families above may share these examples — you do not need a separate demo per comparison or per logical; they all behave the same way (yield `0`/`1`).

---

## 2.2 Functions

Every function below is available in every formula. Arity (argument count) is checked at load — a wrong count fails the file with a clear message. Angles are in **radians**.

### Trigonometry

| Function | Signature | Domain | Example |
|---|---|---|---|
| `sin` | `sin(x)` | any x | `sin(t)` |
| `cos` | `cos(x)` | any x | `cos(u * tau)` |
| `tan` | `tan(x)` | any x | `tan(t * 0.2)` |
| `asin` | `asin(x)` | x ∈ −1..1 (else → 0) | `asin(wave(u))` |
| `acos` | `acos(x)` | x ∈ −1..1 (else → 0) | `acos(x)` |
| `atan` | `atan(x)` | any x | `atan(y / x)` |
| `atan2` | `atan2(y, x)` | any (handles x = 0) | `atan2(y, x)` |

`atan2(y, x)` returns the angle of the point `(x, y)` in −π..π — the safe way to get an angle from coordinates (it picks the right quadrant and never divides by zero).

### Exponential & logarithmic

| Function | Signature | Domain | Example |
|---|---|---|---|
| `exp` | `exp(x)` | any x | `exp(-age)` (decay curve) |
| `log` | `log(x)` | x > 0 (else → 0) | `log(1 + energy * 9)` |
| `log2` | `log2(x)` | x > 0 (else → 0) | `log2(freq)` |
| `log10` | `log10(x)` | x > 0 (else → 0) | `log10(1 + 99 * energy)` |
| `pow` | `pow(x, y)` | (= `x ^ y`) | `pow(energy, 2)` |
| `sqrt` | `sqrt(x)` | x ≥ 0 (else → 0) | `sqrt(x*x + y*y)` |

`pow(x, y)` and `x ^ y` are the same operation; use whichever reads better. Out-of-domain inputs (log of zero, sqrt of a negative) collapse to `0` rather than producing NaN — see [NaN containment](#26-nan--error-containment).

### Shaping

These are the workhorses for remapping values into the look you want.

| Function | Signature | Meaning | Example |
|---|---|---|---|
| `abs` | `abs(x)` | absolute value | `abs(wave(u))` |
| `sign` | `sign(x)` | −1 / 0 / +1 | `sign(x)` |
| `floor` | `floor(x)` | round down | `floor(i / 8)` (grid row) |
| `ceil` | `ceil(x)` | round up | `ceil(energy * 8)` |
| `round` | `round(x)` | nearest integer | `round(t)` |
| `fract` | `fract(x)` | fractional part | `fract(t * 0.5)` (sawtooth) |
| `min` | `min(a, b)` | smaller of two | `min(energy, 0.8)` (cap) |
| `max` | `max(a, b)` | larger of two | `max(flash * 0.9, beat)` |
| `clamp` | `clamp(x, lo, hi)` | constrain to lo..hi | `clamp(age, 0, 1)` |
| `mix` | `mix(a, b, k)` | linear blend, k ∈ 0..1 | `mix(0.2, 1.0, energy)` |
| `smoothstep` | `smoothstep(e0, e1, x)` | smooth 0→1 ramp between e0 and e1 | `smoothstep(0.02, 0.2, energy)` |
| `step` | `step(edge, x)` | `0` if `x < edge`, else `1` | `step(0.5, u)` |

Three to internalize:

- **`mix(a, b, k)`** is a lerp: `k = 0` gives `a`, `k = 1` gives `b`, in between is the straight-line blend. `mix(lowColor, highColor, energy)` crossfades between two looks by loudness.
- **`smoothstep(e0, e1, x)`** is the calm-silence and ease-in tool: below `e0` it is `0`, above `e1` it is `1`, and between it eases with an S-curve (no hard edge). `smoothstep(0.02, 0.2, energy)` is the idiomatic luminance gate — black in silence, smoothly up as sound arrives. It is used in nearly every example in this manual.
- **`step(edge, x)`** is the hard version (no easing) — useful for a meter fill: `step(level, u)` lights segment `u` once the level passes it.

### Conditional

| Function | Signature | Meaning |
|---|---|---|
| `if` | `if(cond, then, else)` | returns `then` when `cond >= 0.5`, else `else` |

```json
"v": "if(settings.glow, 0.35 + 0.65 * band(u), 0.2 + 0.5 * band(u))"
```

> ⚠️ **Both branches are always evaluated.** `if` is pure dataflow — it computes `then` *and* `else` every time, then picks one. It does **not** short-circuit. So `if` cannot protect a risky sub-expression by "not running" it. The danger is division: `if(d != 0, n / d, 0)` still evaluates `n / d` even when `d == 0`. That particular case is safe here only because division by zero is contained to `0` (see [§2.6](#26-nan--error-containment)) — but the **guard idiom** for anything you genuinely want to skip is to neutralize the operand, e.g. `n / max(abs(d), 1e-6)`, or `n * (d != 0) / (d + (d == 0))`. When unsure, make every branch finite on its own.

### Audio lookup

| Function | Signature | Range | Meaning |
|---|---|---|---|
| `band` | `band(u)` | 0..1 | spectrum magnitude at normalized position `u ∈ 0..1` across 48 log-spaced bands (interpolated) |
| `wave` | `wave(u)` | −1..1 | waveform sample at `u ∈ 0..1` across the current 256-sample window (interpolated) |

`band` and `wave` take **any** expression in 0..1 — the argument does not have to be the element's `u`. `band(0.5)` samples the mid-spectrum from anywhere (even a `field` cell or a single instance). `band(u)` maps element position onto the spectrum (low frequencies on the left, highs on the right). `wave(u)` is the oscilloscope.

**Exact-index recipes** (when you want one specific bin rather than a smooth sweep):

- exact band `k` (0..47): **`band(k / 47)`** — e.g. the lowest band is `band(0)`, the highest is `band(47/47)` = `band(1)`.
- exact sample `k` (0..255): **`wave(k / 255)`** — e.g. the first sample is `wave(0)`, the last is `wave(1)`.

See [`ref-polyline.artifact.json`](./examples/ref-polyline.artifact.json) for a `wave(u)` sweep and [`ref-instanced.artifact.json`](./examples/ref-instanced.artifact.json) plus the [spectrum-bars built-in](./05-gallery.md) for `band(u)`.

### Random

| Function | Signature | Range | Meaning |
|---|---|---|---|
| `rand` | `rand(k)` | 0..1 | deterministic hash of (this artifact's seed, `k`); uniform |

> **`rand(k)` is *stable*: the same `k` returns the same value on every frame.** It is not a per-frame dice roll — it is a reproducible hash. This is what makes per-element randomness work.

- **Per-element stable values**: fold the index into `k`. `rand(i)` gives element `i` a fixed value (a star's position, a particle's angle) that does not flicker frame to frame. Need a *second* independent value for the same element? Offset the key: `rand(i + 100)`, `rand(i + 200)`, etc.
- **Re-rolling over time is opt-in**: fold a time-derived term into `k`. `rand(i + beat_count * 1000)` re-rolls every element's value on each beat — the particle-burst idiom (new directions per explosion). Nothing re-rolls unless you ask it to.

See [`ref-rand.artifact.json`](./examples/ref-rand.artifact.json) for a steady star field built entirely from `rand(i + offset)`. Normative details: [contract §2.2](../reference/artifact-contract.md#22-functions).

---

## 2.3 Constants

| Constant | Value | Use |
|---|---|---|
| `pi` | 3.14159… | half a turn in radians |
| `tau` | 6.28318… (= 2π) | a full turn — `cos(u * tau)`, `sin(u * tau)` sweep a whole circle as `u` goes 0→1 |
| `e` | 2.71828… | base of the natural log |

`tau` is the friendliest angle unit here: multiply a 0..1 value by `tau` and you have swept a full circle.

---

## 2.4 Inputs

Inputs are read-only identifiers the runtime supplies each frame. There are three groups: the **nine shared inputs** (available everywhere), your **settings and vars**, and **per-stage extras**.

### The nine shared inputs (every stage)

| Name | Range | Meaning |
|---|---|---|
| `t` | seconds | audio-derived clock (monotonic; keeps advancing even in silence) |
| `dt` | seconds | time since the previous frame (≈ 1/60 s); audio-derived, never wall-clock |
| `energy` | 0..1 | smoothed overall loudness |
| `low` | 0..1 | low-band aggregate (< 250 Hz) |
| `mid` | 0..1 | mid-band aggregate (250 Hz – 4 kHz) |
| `high` | 0..1 | high-band aggregate (> 4 kHz) |
| `beat` | 0..1 | `1.0` on an onset, exponential decay after |
| `beat_count` | integer | total onsets since the artifact started |
| `aspect` | > 0 | render width / height |

During **silence** the app feeds zeros for `energy`, `low/mid/high`, `band()`, `wave()`, and `beat` — but `t` and `dt` keep advancing (the clock never stops). This is why you gate motion and brightness by audio, not by `t` alone (see the [calm-silence recipe](./04-recipes.md)). There is no `silence` input; sustained zero energy *is* the silence signal. Full audio semantics: [contract §3](../reference/artifact-contract.md#3-audio-semantics).

### Settings and vars

- **Settings** are read with the **`settings.` prefix**: `settings.bars`, `settings.tint_r`. A *bare* `bars` is **not** the setting — only vars are read bare. (How each setting type maps to a value — number, toggle as 0/1, choice as a 0-based index, color expanded into `_r/_g/_b/_a` — is covered in [chapter 3 §3.2](./03-scenes-layers-settings.md#32-settings).)
- **Vars** are read by **bare name**: a var declared as `"low_s"` is read as `low_s`, no prefix. (Their per-frame read semantics — declaration order matters — are in [chapter 3 §3.3](./03-scenes-layers-settings.md#33-state-variables-vars) and [`ref-vars-smoothing.artifact.json`](./examples/ref-vars-smoothing.artifact.json).)

This is the one naming rule to remember: **`settings.foo` for settings, bare `foo` for vars.**

### Per-stage extras

A "stage" is the kind of formula being evaluated. Beyond the nine shared inputs, each stage adds the inputs that make sense for it. Variable `init`/`frame` formulas and layer-level fields (`count`, `points`, `resolution`, `thickness`, `visible`, `feedback.decay`) see only the shared inputs, settings, and vars — they are evaluated once per frame, before any per-element work.

| Stage | Extra inputs | Meaning |
|---|---|---|
| `instanced.element` | `i`, `n`, `u` | `i` = element index 0…n−1; `n` = instance count this frame; `u = i / max(n−1, 1)` ∈ 0..1 |
| `polyline.point` | `i`, `n`, `u` | `i`, `n` as above; `u` = position along the line, 0 at the start, 1 at the end |
| `field.cell` | `x`, `y`, `i`, `n` | `x`, `y` = cell-center coordinates in −1..1 (y up); `i`, `n` = cell index/count |

`u` is the everyday parameter: in `instanced` it spreads elements evenly 0→1 (great for `band(u)`, `cos(u * tau)`, hue sweeps); in `polyline` it walks the strip; in `field` you usually reach for `x`/`y` instead.

### Coordinate system

```
        y = +1  (top)
          │
  x=−1 ───┼─── x=+1
 (left)   │   (right)
        y = −1  (bottom)
```

x ∈ −1..1 left→right, y ∈ −1..1 bottom→top, **independent of window size**. Widths `w` and heights `h` use the same units.

> **The aspect note.** Because the coordinate box is always −1..1 on both axes but the window usually is not square, a naive circle `(cos(a), sin(a))` is drawn as an *ellipse*. To draw a **true circle**, divide every x-distance by `aspect`:
>
> ```json
> "x": "cx + r * cos(a) / aspect",
> "y": "cy + r * sin(a)"
> ```
>
> and likewise keep a shape visually square with `"w": "d / aspect"`, `"h": "d"`. This is the circle idiom — see the [true-circles recipe](./04-recipes.md) and [contract §2.3](../reference/artifact-contract.md#23-inputs-read-only-identifiers).

---

## 2.5 Putting it together

A formula is just these pieces composed. Reading right to left, here is a typical element height from the spectrum-bars look:

```json
"h": "2 * band(u)"
```

`u` (this element's position 0→1) → `band(u)` (the spectrum magnitude there, 0..1) → `2 *` (scale to the full −1..1 height). And a luminance gate that keeps silence calm:

```json
"v": "0.08 + 0.6 * smoothstep(0.02, 0.2, energy)"
```

a small floor (`0.08`) plus a smooth ramp that is `0` until `energy` reaches `0.02` and full by `0.2`. Every example in this manual is built from exactly these moves.

---

## 2.6 NaN & error containment

Math goes wrong sometimes — division by zero, `log(0)`, `sqrt(-1)`, `asin(2)`, `(-1)^0.5`. In this language, **any operation that would produce NaN or ±Infinity is replaced with `0` at that step** (the rest of the formula continues from `0`). Final geometry and color values are then clamped to their documented ranges. The consequence: a faulty formula **degrades visibly** (you see black, or a stuck value) but **can never crash the app**.

This is a safety net, not a license to be sloppy — a contained `0` rarely looks like what you intended. Guard divisions and domains deliberately (the [guard idiom in §2.2](#conditional)) so the result is the value you meant, not an accidental `0`. Normative statement: [contract §2.2](../reference/artifact-contract.md#22-functions) ("Any operation producing NaN or ±Infinity is replaced with 0").

---

Prev: [1. Getting Started](./01-getting-started.md) · Next: [3. Scenes, Layers & Settings](./03-scenes-layers-settings.md) · [Table of contents](./README.md)
