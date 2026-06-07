# Getting Started — your first artifact in five steps

> Purpose: take you from an empty file to a polished, music-reactive visual, one
> small change at a time. Every step is a complete file you can paste and run.

This is the chapter to read first. You do not need Rust, a build, or any tooling
beyond a text editor and ViewMusic running. We build one artifact — **My Pulse**,
a glowing disc that breathes with the music — and grow it across five steps. At
every step you get the *whole* file (not a diff), so you can paste any step and
see it work immediately.

Two rules guide everything we do, and the manual practices them in every example:

- **Calm on silence.** When the music stops, the audio inputs go to zero but the
  clock keeps ticking. Gate brightness and motion on the sound, so the scene
  settles instead of strobing into the void.
- **Flash-free beats.** Never drive color or brightness straight from the raw
  `beat` input — it snaps to 1.0 and would flash the whole canvas. Feed beats
  through a *smoothed variable* and let that drive geometry.

You will meet both rules in steps 2, 4 and 5 below.

## Where files go

ViewMusic loads user artifacts from:

```text
~/Library/Application Support/io.github.acrive82.viewmusic/artifacts/
```

Save your file there with the double extension `.artifact.json` — for example
`my-pulse.artifact.json`. The app does not watch the folder live; after saving
you tell it to re-scan with the **Reload** button (next section).

> Tip: if the folder does not exist yet, create it. The `~` is your home folder
> (`/Users/<you>`). In Finder, use **Go → Go to Folder…** and paste the path.

## The reload action

The app re-scans the artifacts folder when you click the **Reload** button (the
↻ refresh icon) next to the dropdown in the overlay — saving a file is not
enough on its own. The loop is:

1. **Save** your edited file in the artifacts folder.
2. **Click Reload (↻)** to re-scan the folder. New and changed files are read in.
3. **Pick your artifact** in the dropdown by its `name` to show it.

One wrinkle when you are iterating on the artifact that is **already on screen**:
a reload keeps the running visual untouched (so a re-scan never interrupts the
show), and re-picking the already-selected entry does nothing. To see fresh
edits to the *currently active* artifact, after Reload switch to another artifact
and back — that rebuilds it from the reloaded bytes.

If your file does **not** appear in the dropdown after a reload, it was rejected
at load time.
Nothing is shown in the UI for a rejected file — the reason is written to the log.
See [Troubleshooting](./06-troubleshooting.md) for where the log is and how to
read a rejection diagnostic.

---

## Step 1 — the minimal valid file

The smallest artifact that loads: a version, a name, and one layer with one
shape. We draw a single circle in the center and make it stay put. No music
reaction yet — we are just getting something on screen.

This is the same file as
[`examples/tutorial-step-1.artifact.json`](./examples/tutorial-step-1.artifact.json)
(every step on this page is identical to its file on disk):

```json
{
  "contract": "1.0",
  "meta": {
    "id": "my-pulse",
    "name": "My Pulse",
    "description": "Tutorial step 1 — a single static circle. The minimal valid artifact.",
    "author": "ViewMusic authoring manual"
  },
  "scene": [
    {
      "type": "instanced",
      "shape": "circle",
      "count": 1,
      "element": {
        "x": "0",
        "y": "0",
        "w": "0.4 / aspect",
        "h": "0.4",
        "color": {
          "model": "hsva",
          "h": "210",
          "s": "0.7",
          "v": "0.6",
          "a": "1"
        }
      }
    }
  ]
}
```

> **What changed:** this is the starting point. Three things are required:
> `contract` (the version), `meta` (id + name), and `scene` (at least one layer).
> The layer is `instanced` — it stamps `count` copies of one `element`. With
> `count: 1` we get exactly one circle.
>
> Note `"w": "0.4 / aspect"` paired with `"h": "0.4"`. The coordinate space runs
> −1..1 on both axes regardless of window shape, so a naive equal width/height
> would draw an *ellipse* on a wide window. Dividing the width by `aspect`
> (= width ÷ height) keeps the circle round. This is the **circle idiom** and you
> will use it constantly.

**What you should see:** a blue, slightly dim disc sitting still in the middle of
the window. It does not react to music yet — that is next.

---

## Step 2 — make it react to loudness

Now we wire the disc's size to `energy`, the smoothed overall loudness (0..1).
The formula `0.2 + 0.6 * energy` means "0.2 units wide in silence, up to 0.8 when
the music is loud."

We also gate the brightness on energy, so the disc dims toward calm when the
music stops — our **calm on silence** rule in action.

Same file as
[`examples/tutorial-step-2.artifact.json`](./examples/tutorial-step-2.artifact.json):

```json
{
  "contract": "1.0",
  "meta": {
    "id": "my-pulse",
    "name": "My Pulse",
    "description": "Tutorial step 2 — the circle's size now tracks loudness (energy).",
    "author": "ViewMusic authoring manual"
  },
  "scene": [
    {
      "type": "instanced",
      "shape": "circle",
      "count": 1,
      "element": {
        "x": "0",
        "y": "0",
        "w": "(0.2 + 0.6 * energy) / aspect",
        "h": "0.2 + 0.6 * energy",
        "color": {
          "model": "hsva",
          "h": "210",
          "s": "0.7",
          "v": "0.25 + 0.6 * smoothstep(0.02, 0.2, energy)",
          "a": "1"
        }
      }
    }
  ]
}
```

> **What changed:**
> - `w` and `h` now read `energy`: `(0.2 + 0.6 * energy) / aspect` and
>   `0.2 + 0.6 * energy`. The disc grows with loudness. (Width still divided by
>   `aspect` to stay round.)
> - `v` (brightness) became
>   `0.25 + 0.6 * smoothstep(0.02, 0.2, energy)`. `smoothstep(0.02, 0.2, energy)`
>   ramps from 0 to 1 as energy crosses a small threshold — a clean fade-in that
>   sits at a calm 0.25 during silence instead of going fully dark or flashing.

**What you should see:** the disc pumps with the music — bigger and brighter on
loud passages, small and dim when the track is quiet or stops.

---

## Step 3 — add an adjustable color

Let's give the listener a knob. We add a **color setting** named `tint`. Settings
auto-render as controls in the top-right panel: a `color` setting becomes a color
picker. In formulas, a color setting exposes four channels —
`settings.tint_r`, `settings.tint_g`, `settings.tint_b`, `settings.tint_a`
(each 0..1) — so here we switch the disc to the `rgba` color model and multiply
each channel by our energy-gated brightness.

Same file as
[`examples/tutorial-step-3.artifact.json`](./examples/tutorial-step-3.artifact.json):

```json
{
  "contract": "1.0",
  "meta": {
    "id": "my-pulse",
    "name": "My Pulse",
    "description": "Tutorial step 3 — a color setting drives the disc, still gated by energy.",
    "author": "ViewMusic authoring manual"
  },
  "settings": {
    "tint": {
      "type": "color",
      "label": "Disc color",
      "default": "#3aa0ff"
    }
  },
  "scene": [
    {
      "type": "instanced",
      "shape": "circle",
      "count": 1,
      "element": {
        "x": "0",
        "y": "0",
        "w": "(0.2 + 0.6 * energy) / aspect",
        "h": "0.2 + 0.6 * energy",
        "color": {
          "model": "rgba",
          "r": "settings.tint_r * (0.25 + 0.6 * smoothstep(0.02, 0.2, energy))",
          "g": "settings.tint_g * (0.25 + 0.6 * smoothstep(0.02, 0.2, energy))",
          "b": "settings.tint_b * (0.25 + 0.6 * smoothstep(0.02, 0.2, energy))",
          "a": "1"
        }
      }
    }
  ]
}
```

> **What changed:**
> - A new top-level `settings` block with one `color` setting, `tint`, defaulting
>   to `#3aa0ff`. It shows up as a color picker labelled "Disc color".
> - The color model changed from `hsva` to `rgba`, and each of `r`/`g`/`b` is the
>   chosen channel (`settings.tint_r`…) times the **same** energy-gated brightness
>   factor we used before. The color is the listener's; the *pulse and the calm*
>   are still ours.
>
> Reading settings: always with the `settings.` prefix. A bare `tint_r` would not
> be the setting — bare names are reserved for `vars` (next step) and the built-in
> inputs.

**What you should see:** the same breathing disc, now in whatever color you pick
in the panel. Changing the picker updates it within a frame.

---

## Step 4 — smooth the motion with a variable

Driving size straight from `energy` is a little twitchy — `energy` jiggles frame
to frame. We can make the pulse *feel* better with a **state variable** that holds
its peak and eases back down: the **peak-hold** idiom.

A `vars` entry has an `init` (run once) and a `frame` formula (run every frame).
Ours is:

```text
pulse = max(pulse * 0.9, energy)
```

Each frame, `pulse` decays to 90% of itself — unless the current `energy` is
higher, in which case it jumps straight up to it. The result snaps up on a hit and
glides back down: an organic, asymmetric envelope instead of raw jitter. We then
drive `w`, `h`, and `v` from `pulse` instead of `energy`.

Same file as
[`examples/tutorial-step-4.artifact.json`](./examples/tutorial-step-4.artifact.json):

```json
{
  "contract": "1.0",
  "meta": {
    "id": "my-pulse",
    "name": "My Pulse",
    "description": "Tutorial step 4 — a smoothing var (pulse) drives the size for a softer, hold-and-decay feel.",
    "author": "ViewMusic authoring manual"
  },
  "settings": {
    "tint": {
      "type": "color",
      "label": "Disc color",
      "default": "#3aa0ff"
    }
  },
  "vars": {
    "pulse": { "init": "0", "frame": "max(pulse * 0.9, energy)" }
  },
  "scene": [
    {
      "type": "instanced",
      "shape": "circle",
      "count": 1,
      "element": {
        "x": "0",
        "y": "0",
        "w": "(0.2 + 0.6 * pulse) / aspect",
        "h": "0.2 + 0.6 * pulse",
        "color": {
          "model": "rgba",
          "r": "settings.tint_r * (0.25 + 0.6 * smoothstep(0.02, 0.2, pulse))",
          "g": "settings.tint_g * (0.25 + 0.6 * smoothstep(0.02, 0.2, pulse))",
          "b": "settings.tint_b * (0.25 + 0.6 * smoothstep(0.02, 0.2, pulse))",
          "a": "1"
        }
      }
    }
  ]
}
```

> **What changed:**
> - A new `vars` block declares `pulse` with
>   `"frame": "max(pulse * 0.9, energy)"`. Note `pulse` reads *itself* — in a
>   `frame` formula, reading a var's own name gives last frame's value, which is
>   exactly what a decay needs.
> - Everywhere we read `energy` for geometry/brightness we now read `pulse`.
>
> **Why smoothed feels better:** raw `energy` reacts instantly but also *drops*
> instantly, so the disc flickers on transients. `pulse` keeps the high-water mark
> and releases it gently, so a kick reads as a satisfying swell-and-settle rather
> than a stutter. This same trick — a smoothed var standing in front of a raw
> input — is how we get **flash-free beats**: never wire `beat` straight to a
> visual; route it through a var like `max(flash * 0.9, beat)` and drive geometry
> from that.

**What you should see:** the same pulsing disc, but the motion is noticeably
silkier — it surges on hits and eases back instead of buzzing.

---

## Step 5 — polish

Time to make it feel finished. We add three touches, each building on what we
have:

1. **Hue from the spectrum.** The main disc moves to the `hsva` model and reads
   its hue from `band(0.2)` — the loudness near the low-mid part of the spectrum —
   so the color shifts with the music's texture.
2. **Gentle idle motion, gated by energy.** A second var, `drift`, slowly advances
   an angle; we orbit the disc a tiny bit using `cos(drift)`/`sin(drift)`. The
   advance speed is `0.2 + 1.5 * energy`, so it nearly stops on silence — alive,
   not restless.
3. **A second accent layer.** A small bright core, in the listener's `tint` color,
   that pops on the pulse. Both layers use `"blend": "add"` so overlaps glow.

Same file as
[`examples/tutorial-step-5.artifact.json`](./examples/tutorial-step-5.artifact.json):

```json
{
  "contract": "1.0",
  "meta": {
    "id": "my-pulse",
    "name": "My Pulse",
    "description": "Tutorial step 5 — polished: hue from the spectrum, gentle energy-gated idle motion, and a second accent layer.",
    "author": "ViewMusic authoring manual"
  },
  "settings": {
    "tint": {
      "type": "color",
      "label": "Accent color",
      "default": "#ffcc44"
    }
  },
  "vars": {
    "pulse": { "init": "0", "frame": "max(pulse * 0.9, energy)" },
    "drift": { "init": "0", "frame": "drift + dt * (0.2 + 1.5 * energy)" }
  },
  "scene": [
    {
      "type": "instanced",
      "shape": "circle",
      "blend": "add",
      "count": 1,
      "element": {
        "x": "0.06 * cos(drift) / aspect",
        "y": "0.06 * sin(drift)",
        "w": "(0.2 + 0.6 * pulse) / aspect",
        "h": "0.2 + 0.6 * pulse",
        "color": {
          "model": "hsva",
          "h": "200 + 120 * band(0.2)",
          "s": "0.75",
          "v": "0.25 + 0.6 * smoothstep(0.02, 0.2, pulse)",
          "a": "1"
        }
      }
    },
    {
      "type": "instanced",
      "shape": "circle",
      "blend": "add",
      "count": 1,
      "element": {
        "x": "0",
        "y": "0",
        "w": "(0.05 + 0.12 * pulse) / aspect",
        "h": "0.05 + 0.12 * pulse",
        "color": {
          "model": "rgba",
          "r": "settings.tint_r * (0.2 + 0.8 * pulse)",
          "g": "settings.tint_g * (0.2 + 0.8 * pulse)",
          "b": "settings.tint_b * (0.2 + 0.8 * pulse)",
          "a": "0.9"
        }
      }
    }
  ]
}
```

> **What changed:**
> - The main disc's `h` is now `200 + 120 * band(0.2)` — color tied to the
>   spectrum — and it moved to `hsva` so we can set hue directly.
> - A `drift` var (`drift + dt * (0.2 + 1.5 * energy)`) advances an angle; the
>   disc's `x`/`y` orbit it slightly. Because the speed scales with `energy`, the
>   drift slows to a near-standstill in silence (calm on silence again). Using
>   `dt` to advance makes the speed frame-rate-independent.
> - A second `instanced` layer draws a small core in the `tint` color, scaled and
>   brightened by `pulse`. Both layers are `"blend": "add"`.
> - The `tint` setting was relabelled "Accent color" since it now drives the core.

**What you should see:** a glowing disc that breathes with the beat, shifts hue
with the music, slowly orbits while sound is playing (and rests when it stops),
with a bright accent core in your chosen color punching on each pulse.

You have a complete, polished, music-reactive artifact — built from the same five
moves you will reuse for everything else: a shape, a reaction, a setting, a
smoothed var, and polish.

---

## Where to go next

- **[Formula language reference](./02-formula-language.md)** — every operator,
  function, constant, and input, each with an example.
- **[Scenes, layers & settings](./03-scenes-layers-settings.md)** — the full
  structure: all three layer types, all four setting types, colors, vars, and
  feedback.
- **[Recipes](./04-recipes.md)** — copy-paste patterns for true circles,
  particles, trails, peak-hold meters, calm silence, and flash-free beats.
- **[Gallery](./05-gallery.md)** — every shipped built-in explained, with its key
  techniques called out.
- **[Troubleshooting](./06-troubleshooting.md)** — the authoring loop end to end,
  and how to read a rejection diagnostic when a file does not show up.
- For the exact rules (limits, version policy, the legal definition of every
  feature) the manual teaches but never overrides, see the normative
  [artifact contract](../reference/artifact-contract.md).

---

⟵ prev · [Table of contents](./README.md) · [Formula language reference](./02-formula-language.md) ⟶
