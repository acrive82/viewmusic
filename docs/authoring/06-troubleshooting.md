# Troubleshooting — the authoring loop and reading diagnostics

> Purpose: close the loop. When a file does not show up, this chapter shows you
> where it went, how to read the one-line reason in the log, and the fix for every
> common mistake.

ViewMusic never pops up an error for a bad artifact. A rejected file simply does
not appear in the dropdown — quietly, so the live show is never interrupted. The
*reason* is always written to the log. Once you know where the log is and how a
rejection line is shaped, fixing your own files becomes a 10-second loop.

## The authoring loop, end to end

```text
1. Edit your file        ~/Library/Application Support/io.github.acrive82.viewmusic/artifacts/<name>.artifact.json
2. Save
3. Click Reload (↻)     the refresh button beside the dropdown re-scans the folder
4. Pick your artifact   select it by name in the dropdown
        ├─ it appears and renders        → you're done
        └─ it does NOT appear            → it was rejected; read the log ↓
5. Read the log          ~/Library/Logs/io.github.acrive82.viewmusic/viewmusic.log
6. Fix the named spot, save, reload      → back to step 3
```

- **Artifacts folder:**
  `~/Library/Application Support/io.github.acrive82.viewmusic/artifacts/`
  Files must end in `.artifact.json`. The folder is **not** watched live — after
  saving, click **Reload** to re-scan (no app restart needed).
- **The reload action:** click the **Reload** button (the ↻ refresh icon) next to
  the dropdown to re-scan the folder, then select your artifact by its `name`.
  When you are editing the artifact that is **already on screen**, a reload leaves
  the running visual untouched and re-picking the active entry is a no-op — so to
  pull in fresh edits to the active artifact, switch to another and back after
  reloading.
- **The log:** `~/Library/Logs/io.github.acrive82.viewmusic/viewmusic.log`
  Every rejection is one English line naming the file, the step, the JSON path,
  and the offending token. Open it in any editor, or tail it while you iterate.

> Tip: keep the log open in a second window. The newest rejection is the last
> line — it tells you exactly what to fix.

## Anatomy of a rejection diagnostic

Every rejection line has the same shape:

```text
file <name>: [<step>] at <json_path>: <message>
```

| Field | What it tells you |
|---|---|
| `file` | which artifact file failed (the name you saved) |
| `step` | how far it got before being rejected — see the ladder below |
| `json_path` | exactly where in your JSON the problem is, e.g. `scene[0].element.color.h` |
| `message` | the English reason, naming the offending **token** when known |

The **step** is the validation stage that stopped the file. They run in order, and
the first one to fail wins, so the step tells you the *kind* of problem:

| Step | Meaning |
|---|---|
| `parse` | the file is not valid JSON (or exceeds the file-size limit) |
| `version` | the `contract` field is missing/malformed or unsupported |
| `schema` | the JSON shape is wrong — missing required key, wrong type, or an **unknown key** (strict mode: typos like `"colour"` are rejected, not ignored) |
| `deserialize` | the JSON is shaped right but a value can't become the typed field |
| `semantic` | a rule about values — bad id, setting `min ≥ max`, a name colliding with a reserved identifier, etc. |
| `formula` | a formula failed to compile — unknown identifier, stage misuse, syntax error, or an op-count/length cap |

## Real-shaped examples and their fixes

These are the kinds of lines you will actually see. Each is followed by the fix.

### 1. A bare setting name (missing the `settings.` prefix)

```text
file my-pulse.artifact.json: [formula] at scene[0].element.w: unknown identifier 'tint_r' at position 0
```

A `color` setting publishes its channels as `settings.tint_r`, `settings.tint_g`,
`settings.tint_b`, `settings.tint_a` — **with the `settings.` prefix**. A bare
`tint_r` is not declared anywhere, so the compiler can't resolve it.

**Fix:** add the prefix.

```diff
- "w": "tint_r * 0.5"
+ "w": "settings.tint_r * 0.5"
```

Bare names are reserved for `vars` (read by their plain name) and the built-in
inputs (`energy`, `t`, `aspect`, …). Settings are *always* `settings.<name>`.

### 2. A stage extra used in the wrong stage (`x` in an element)

```text
file my-pulse.artifact.json: [formula] at scene[0].element.x: identifier 'x' is not available in this stage (element/point)
```

`x` and `y` are the **cell-center coordinates** of a `field` layer; they exist
only in `field` `cell` formulas. In an `instanced` element you set the element's
position by *writing* the `x`/`y` fields — you don't read an input called `x`.

**Fix:** build position from inputs that *are* available in the element stage —
`u` (the element's 0..1 position), `i`, `n`, plus audio/time:

```diff
- "x": "x * 0.5"
+ "x": "-1 + 2 * u"
```

(`u`, `i`, `n` are available in `instanced.element` and `polyline.point`; `x`, `y`
only in `field.cell`.)

### 3. A `choice` setting compared as a string

A `choice` setting is **a number in formulas** — the 0-based index of the selected
option, not the option's text. There are no string literals in the formula
language, so writing a quote at all stops the lexer cold:

```text
file my-pulse.artifact.json: [formula] at scene[0].element.w: unexpected character '"' at position 19
```

**Fix:** compare to the index. If `"options": ["thin", "wide"]`, then `"thin"` is
`0` and `"wide"` is `1`:

```diff
- "w": "if(settings.style == \"wide\", 1.6 / n, 0.8 / n)"
+ "w": "if(settings.style == 1, 1.6 / n, 0.8 / n)"
```

### 4. An unknown key (a typo the loader refuses to ignore)

```text
file my-pulse.artifact.json: [schema] at scene[0].element: schema validation failed: additional properties are not allowed ('colour' was unexpected)
```

The loader runs in **strict mode**: a misspelled or unknown key is rejected rather
than silently doing nothing, so authoring mistakes stay loud. Schema-step messages
always begin with `schema validation failed:` and the `json_path` points at (or
just above) the offending key.

**Fix:** spell it the way the contract spells it — `color`, not `colour`.

## Other common mistakes

- **`count` "turning a layer off" by going to 0.** An `instanced` layer's `count`
  is floored then **clamped to a minimum of 1** — a `count` that evaluates to `0`
  still draws one instance. To make a layer disappear, gate it with `visible`
  (a `visible` formula below 0.5 skips the whole layer for that frame), not with
  `count`.

  ```diff
  - "count": "settings.showRing"          // 0 still draws 1 — won't hide it
  + "count": "64",
  + "visible": "settings.showRing"        // < 0.5 skips the layer
  ```

- **Exceeding a limit.** The loader enforces caps at load time, so a reader copying
  your file never hits a cap mid-show. If you trip one you'll see a `semantic` or
  `formula` line, e.g.:

  ```text
  file my-pulse.artifact.json: [formula] at scene[3].cell.color.h: artifact exceeds the 16384 total-operation limit
  ```

  (the path names the formula that tipped the total over the cap.)

  Keep within: 16 layers per scene; 4096 instances/points per layer; `field`
  resolution ≤ 128; ≤ 32 settings and ≤ 64 vars; ≤ 1024 characters and ≤ 256
  operations per formula; ≤ 16 384 operations total; file ≤ 256 KiB. (Full table:
  the contract's [§12 Limits](../reference/artifact-contract.md#12-limits-summary-load-time-enforced).)
  Design with margin — simplify a heavy formula, or reduce `count`/`resolution`.

- **Driving color or brightness from raw `beat`.** Not an error, but it breaks the
  **flash-free** rule: `beat` snaps to 1.0 and would strobe the canvas. Route it
  through a smoothed var (`max(flash * 0.9, beat)`) and drive *geometry* from that.
  See the [flash-free beats recipe](./04-recipes.md).

- **Animating on `t` alone, so silence never rests.** During silence the audio
  inputs go to zero but `t`/`dt` keep advancing. If your motion reads only `t`, the
  scene keeps moving in dead air. Gate motion/brightness on `energy` (or
  `smoothstep(0.02, 0.1, energy)`) — the **calm on silence** rule. See the
  [calm-on-silence recipe](./04-recipes.md).

## The `.app`-launch / permissions caveat

ViewMusic reads the **system audio mix**, which on macOS requires a permission the
OS grants to a properly launched application bundle. Launch the app by opening
**ViewMusic.app** the normal way (double-click in Finder, or from
`/Applications`). If you start the binary some other way (for example invoking the
inner executable directly from a terminal), macOS may not associate it with the
app's permission grant, and you can end up with **permanent silence** — the
visuals run but every audio input stays at zero, which (correctly) renders the
calm idle look. If your artifact loads and shows its idle state but never reacts to
music, confirm:

1. You launched **ViewMusic.app** itself (not the inner binary).
2. Audio capture permission is granted in **System Settings → Privacy & Security**.
3. Something is actually playing through the system output you're capturing.

---

⟵ [Gallery](./05-gallery.md) · [Table of contents](./README.md) · next ⟶
