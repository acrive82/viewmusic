# ViewMusic Authoring Manual

> Purpose: teach you to draw your own music-reactive visuals for ViewMusic — from
> an empty file to a polished, shippable artifact — entirely in JSON.

This is the friendly, example-first companion to the
[artifact contract](../reference/artifact-contract.md).
The contract is the law (every exact rule, limit, and signature); this manual is
the guide that teaches it, with a tutorial, a complete reference, copy-paste
recipes, and an explained gallery of every shipped built-in. Wherever the two
ever disagree, **the contract wins and this manual is the bug** — so the manual
links to the contract for normative questions rather than restating its legal
text.

## Who this is for

You are comfortable editing JSON in a text editor. You do **not** need Rust, a
build, a compiler, or any tooling — just ViewMusic running and an editor. An
artifact is one `.artifact.json` file whose geometry, motion, and color are
little math formulas over the live audio (a 48-band spectrum, the waveform,
loudness, beat events), time, and your own settings. You write the file; the app
draws it.

## The two golden rules

Every example in this manual obeys these, and so should everything you make:

1. **Calm on silence.** When the music stops, the audio inputs go to zero but the
   clock keeps ticking. *Gate brightness and motion on the audio* (the idiom is
   `smoothstep(0.02, 0.12, energy)`), so a silent scene settles into a calm idle
   instead of strobing or churning over dead air.
2. **Flash-free beats.** Never wire the raw `beat` input to a color or brightness
   channel — it snaps to `1.0` on every onset and would strobe the whole canvas.
   Route the beat through a *smoothed variable* and let that drive **geometry**
   (size, radius, length, position): a beat is *felt as a punch*, not *seen as a
   flash*.

These two rules show up in the [tutorial](./01-getting-started.md) (steps 2, 4,
5) and have their own recipes in chapter 4
([calm on silence](./04-recipes.md#45-calm-on-silence),
[flash-free beats](./04-recipes.md#46-flash-free-beats)).

## How to use this manual

- **First time?** Read [1. Getting Started](./01-getting-started.md) top to bottom
  — five small steps, each a complete file you can paste and run.
- **Looking something up?** Chapters [2](./02-formula-language.md) and
  [3](./03-scenes-layers-settings.md) are references; jump to the table you need.
- **Building something specific?** Steal a pattern from
  [4. Recipes](./04-recipes.md) or a whole built-in from
  [5. Gallery](./05-gallery.md).
- **Stuck — your file won't show up?** [6. Troubleshooting](./06-troubleshooting.md)
  decodes the log and the workflow.

Every complete example lives in [`examples/`](./examples/) as a standalone
`<kind>-<name>.artifact.json` and is loaded through the real validation pipeline
by an automated test on every build — so nothing in this manual can quietly drift
out of date.

## Table of contents

| Chapter | What it covers |
|---|---|
| [1. Getting Started](./01-getting-started.md) | A five-step tutorial from an empty file to a polished, music-reactive disc — shape → react to loudness → add a setting → smooth it with a var → polish. Each step is a complete, paste-and-run file. |
| [2. The Formula Language](./02-formula-language.md) | The little expression language behind every value: operators and precedence, every function and constant, the inputs (audio/time/settings/vars), per-stage extras, the coordinate system, and NaN containment — each with examples. |
| [3. Scenes, Layers & Settings](./03-scenes-layers-settings.md) | The JSON structure: the artifact skeleton, all four setting types, state variables and their read semantics, the three layer types field by field, color models, blend modes, feedback trails, the limits, and contract versioning. |
| [4. Recipes](./04-recipes.md) | Six proven, copy-paste idioms with the formulas explained line by line: true circles, closed-form particles, feedback trails, peak-hold meters, calm on silence, and flash-free beats. |
| [5. Gallery](./05-gallery.md) | Every shipped built-in explained on a fixed schema — what you see, what drives it, the key techniques (linked to the recipe that teaches each), and the settings worth turning. |
| [6. Troubleshooting](./06-troubleshooting.md) | The edit → save → reload loop, where the log lives, how to read a rejection diagnostic (file, step, JSON path, token), the common mistakes, and the audio-permission caveat. |

## The last word: the contract

For the exact, normative definition of every feature — limits, version policy,
the legal meaning of each field — see the
[artifact contract](../reference/artifact-contract.md).
This manual teaches and exemplifies it; it never overrides it.

---

Start here → [1. Getting Started](./01-getting-started.md)
