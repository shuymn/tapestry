# Stage 3 Design: Skins

Stage 3 of the roadmap (design.md "Roadmap") is the look and effects of the play screen. This file plans the skin system: the runtime model the player draws from, the importers that make existing skins playable, tapestry's own skin format, and its export to beatoraja-family players.

**Status.** Decided; the decisions that change design.md are in it ("Goals", "Rendering and Skins", "Custom Skin Format", "beatoraja Lua Skins", "Extension Boundaries", "Settings Scopes", "Language and Crate Structure", "Libraries", "Roadmap", "Open Questions"). The fast and slow counts in `Tally` go into design.md with the evaluator change that adds them (S9). This file holds the plan and the details, and is removed when stage 3 exits.

**Citations.** `file:N` points into the repository at commit 17e17de unless it names another revision, and design.md is cited by section name. "ED" is LR2oraja Endless Dream; "upstream" is beatoraja itself. What is said about their skin loaders was read from their published source as a description of the file format; none of their code is ported (design.md "Libraries").

**What the skin system must do:**
- **Existing skins first.** Skins made for beatoraja-family players become playable in tapestry before tapestry's own format exists. The importers are separate from the format and never shape it.
- **A small format of its own.** Few people make skins today. A spec that is large, or has features an author gains nothing from, goes unused. Every concept an author learns has to pay off for them.
- **One source, two skins.** A skin written in tapestry's format also builds a beatoraja skin, so one piece of work reaches the players of both. The beatoraja skin is a faithful subset: tapestry's may do more, and what beatoraja cannot express degrades in the export and never breaks it.
- **Not bound to 16:9.** The format adapts to other aspect ratios such as 21:9. The beatoraja export stays 16:9.
- **The player adjusts the composition.** A home setup is not a cabinet. Players move their monitor or where they sit because a skin fixes the whole layout and the existing players can only stretch the screen. Here the player moves and resizes the parts of the screen, and the skin author does nothing for it.
- **Crisp at any size.** Image skins blur or get jaggy when stretched or shrunk. Scaling is handled at load, so it costs nothing per frame.

**Direction applied throughout:**
- **Two levels.** A broad compiled *runtime model* is what the player draws from; the importers and the compiler of the small *authored format* both target it. The runtime model grows only with the PR that first needs a feature, which for import-only features is an importer.
- **The authored format is specified on its own terms.** Its semantics are written for tapestry; the beatoraja export is a constraint every construct must meet (it has an export rule), never the source of the semantics.
- **The logic thread never knows skins exist.** Everything a skin shows comes from the play view, the effect queue and the chart, as today (design.md "Rendering and Skins").
- **Nothing is built ahead of the PR that needs it,** and skin work before stage 2 exits lives in new portable crates that touch neither `render`, `player`, `game` nor `eval`.

## 1. Scope

**In:**
- **The runtime model and its evaluation** in portable crates: elements, keyframe tracks, timers, typed expressions, regions, widgets, the atlas.
- **The beatoraja importer** (JSON and Lua skins) into the runtime model, before the authored format.
- **The authored format**, its compiler and diagnostics.
- **The beatoraja exporter** and an authoring CLI that checks, previews, and builds both skins in one command.
- **Player layout adjustment**: per-region position, scale and visibility, lane width, and an adjust mode drawn with the play screen's sprites.
- **The standard skin**, written in the format with self-made art, and the removal of `SimpleSkin`.
- **BGA**, starting from the stage 0 spike's decoder.

**Out:**
- **The LR2 importer**, which comes right after stage 3 (§7, decision 1).
- **Graphs and visualizers** (score graph, judgment graph, hit-error), double play, mouse dragging in the adjust mode (it comes with `egui`), SVG images, SDF shapes, MSDF text.
- **Other screens' skins** (song selection, results). Stage 3 skins the play screen only.

## 2. The runtime model (`skin-model`)

### R1. Elements, tracks and draw order

A compiled skin is a list of elements in draw order. Each element has a source (an image frame, a number, text, a bar, or a widget), a keyframe track, a timer, a visibility condition, a blend and a region. The runtime model carries the union of what imported skins need: five blends (alpha, add, subtract, multiply, invert), quadratic and cubic eases, per-object or per-keyframe ease, clip rectangles, and derived edge timers. Each of these enters with the PR that first needs it: an importer PR for import-only features, the LR2 importer for the LR2-only ones.

**Draw order across blends.** `SpriteBatch` keeps alpha and additive sprites in two lists and draws every additive sprite after every alpha one (crates/render/src/sprite.rs:30-39), so an alpha element over an additive one is drawn in the wrong order. The runtime model's `QuadBatch` is one ordered list with preallocated runs; a run breaks wherever the blend or the clip rectangle changes, so two neighbouring elements with different clips never share a scissor. Render draws the runs in order (§6, "Render"), and the CPU preview applies the same runs.

### R2. Animation

Times are integer milliseconds in files and microseconds inside. For an element with timer `T` and keyframes at `k0 < … < kn`:
- The element is not drawn while `T` is off.
- Local time is `t = now − start(T)`; an element without a timer uses `ready`, so it is drawn during the countdown as the target loaders draw untimed objects from the screen's start.
- After the last key, one of three behaviors, named by the author: **hold** (keep the last key's values), **hide** (drawn while `t ≤ kn`, not after), or **loop from `L`** (`k0 ≤ L < kn`): for `t > L`, `t' = L + (t − L) mod (kn − L)`, so `t = kn` already shows `L`. This matches the target loaders exactly (ED `SkinObject.prepareRegion`), so the export is one-to-one.
- Before `k0` the element is not drawn.
- Between keys, x, y, w, h, alpha, r, g, b and angle interpolate; a field left out of a key keeps the previous key's value. Rotation and scaling use a 9-point pivot.
- Ease per track: linear, in, out, step (in and out are quadratic). Cubic and per-keyframe ease come into the runtime model with the importer that needs them.
- **Image frames** are not part of the track: an image's frame cycle runs on the image's own timer, so an element can show on `ready` while its frames restart on `judge`. With `n` frames over a period `p` (1 µs to the longest key time), `t` after that timer started shows frame `((t mod p) · n) / p`; frame 0 shows while the timer is off, without a cycle, or with one frame. Both timers come from the same frame's fill, before any element is drawn. Whether the target loaders show frame 0 while a cycle's timer is off is checked when S6 imports; the importer adapts, the rule stays.
- In the authored format, keys must be strictly increasing from `k0 ≥ 0` and a loop point must lie inside the track (the target loaders hide a finished track by setting its time to −1, which a negative key would make visible again); anything else is a load error. The target loaders accept unsorted and equal-time keys and apply a loop only when the last key is after 0; the importer reproduces that, and the exporter emits only tracks that need neither. Tests pin `k0`, `kn` and `L` at exactly and ±1 µs, for hold, hide, loop and a single-key track.

### R3. State and timers

The skin reads a per-frame `SkinFrame`, filled from the render thread's `FrameInput`, the chart, a play context given to `prepare`, and a reducer over the drained effects. The play context is fixed for one attempt: the preset's gauge descriptors (`start` and `max`, in the order of `PlayView::gauges`, which carry values only, crates/game/src/render.rs:17-28), the attempt's start position and, in practice, its section, so the lanes widget draws only the section's targets (stage2.md D15) and `ready` and `play` start from the attempt's own countdown, not chart time 0.

**Where each kind of value comes from:**
- **Numbers come only from the play view.** A dropped effect loses a flash, never a number on screen (crates/game/src/render.rs:36-40). Counts, EX score, BP, combo, gauges and the like are read from `PlayView`, never accumulated from effects.
- **Edge timers come from ordered effects.** A timer's value is its start time, taken from the event's chart time, not the frame's.
- **Conditions sample once per frame** after the drained effects are applied. Because the newest snapshot can be ahead of a drain that the effect buffer splits over frames (crates/player/src/render_side.rs:415-421), a condition may see a newer combo beside an older judgment for a frame; this is accepted and documented, and edge timers never derive from snapshot values that effects already carry.

**Feedback is not scoring.** A long note's `Judged` comes at its release or end, so feedback follows the existing play screen rules (crates/render/src/simple.rs:265-335):
- A press feedback (bomb, judgment display) comes from `Judged` at a press, or from `LnHeld` for a long note's head, never from the long note's completion; a completed long note adds its judgment text but no second bomb (the test at simple.rs:1013 is kept).
- A BAD head starts the visual hold through its `Judged`, without `LnHeld`; the hold ends at `LnHoldEnded`.
- `EmptyPoor` drives the empty-POOR feedback.

**Timers in stage 3:** `play`, `ready`, `failed` (from `GaugeState::failed_at`, crates/eval/src/gauge.rs:29, not from a gauge value: a normal gauge at 0 has not failed), `last_note`, `full_combo`, `gauge_max`, `beat` (from the scroll map, on the display clock: the latest integer-beat crossing, which a STOP does not restart); `judge`, `judge.<grade>` (only the latest judgment's grade is on), `combo.up` (the field `combo` holds the count); per lane `press`, `release` (off until the play view carries lane input, S9), `bomb`, `hold`, `mine`; and `when(<condition>)` timers, which start on the condition's rising edge and turn off on its falling edge. A `when` condition may read state and built-in timers but not another `when` timer, so there is no cycle and no order to choose; every `when` timer is updated once per frame before any element is drawn. `gauge_max` reads off while the snapshot's gauge is below its maximum; if a below-maximum effect is dropped and the gauge later refills, it keeps the earlier start. Every timer has a fixed slot sized in `prepare`. The reducer is reset in `prepare`, so a retry starts clean. Its tests split one sequence of effects across drains at every point and compare against explicit expected states, and cover a mid-chart countdown, a practice round and two elements sharing one `when` timer.

**Lane input in the play view** *(coordination PR, §6 S9)*. Key beams and key pads need presses without notes and releases, which no effect carries: `PlaySound` comes only for a press with a target note that has a sound (crates/eval/src/evaluator.rs:927-937). `PlayView` gains, per lane, whether it is held now and the chart times of its latest press and release, filled from the same conversion that feeds the evaluator, so a scratch reversal (a release and a press at one time, crates/eval/src/convert.rs:386-390) and a control held at the start without a press edge (convert.rs:145-150) come out right. Autoplay fills it too. It is generic input state, not skin state.

**Fast/slow** need counts the play view does not have (`Tally`, crates/eval/src/evaluator.rs:122-136): they are added to `Tally` in `eval` (§7, decision 6).

### R4. Expressions

Expressions give conditions and values; they never move or color anything (§4).
- **Types:** `bool`, `int`, and enumerations (judgment grades, gauge kinds). Durations are written `120ms` or `1.5s`. There is no float and no fixed point: values that are fractions are integers in a stated unit (gauge and rate in ‰, BPM ×100), and only an image's on-screen geometry is float, after evaluation.
- **Range-checked integers.** Every state field declares its range, and the compiler bounds every subexpression; an expression that could leave `[−2^31, 2^31 − 1]`, the range of the target's integer properties, is a compile error. Times read by expressions are in milliseconds (`elapsed` is milliseconds, −1 when off and at most 4 h, so a chart's time range fits and `elapsed(t) * 100` stays in range). State reads clamp to the declared range, so a value outside it can never break a bound the compiler proved. Within that bound the native evaluator and the exported Lua, whose numbers are Java `int` or `double`, compute the same results, so arithmetic needs no saturation. `/` and `%` floor and yield 0 for a zero divisor; the export lowers them to guarded prelude functions, since `math.floor(a / b)` alone does not give 0. Tests run both divisor signs and zero through the exported Lua as well as the native evaluator.
- **These rules are the authored language's.** The importer converts a Lua callback only when the native operations give the same result as Lua's: a fractional division whose result is compared (`n / 2 > 0` is true at 1 in Lua and false under floor division) is not converted and falls under §3's unsupported-callback rule.
- **Operators and built-ins:** `+ - * / %`, comparisons, `&& || !`, `if c then a else b`, `min`, `max`, `clamp`, `abs`, `on(timer)`, `elapsed(timer)`, and option tests.
- **Pipeline:** a hand-written lexer and Pratt parser with spans, names resolved against a static schema (name → type, range, slot, export mapping), type and range checks, folding of options and constants, and a postfix program with its stack depth computed. The evaluator runs it on a fixed stack, allocates nothing, and is in `IN_PLAY`.
- **Bounds** (design.md threat model "Skins"): source length (1024 bytes), nesting depth (an operator height of 64, counting `clamp` as its lowering; checked while parsing, before recursion), operations per expression (256) and per skin (65536), and stack depth (32) are capped, so per-frame cost has a bound known at load.
- **Rejected:** `evalexpr` (AGPL since 12.0, allocates), `rhai` and `cel-interpreter` (dynamic, allocating), `fasteval` (unmaintained, float), `exmex` (untyped). A combinator library adds nothing a ~300-line Pratt parser needs.

### R5. Layout: canvas, regions and player adjustment

**Canvas.** The canvas is 1080 reference pixels tall and `1080 × aspect` wide; narrower than 16:9 it is 1920 wide and grows taller instead (§7, decision 3). It is scaled uniformly to the render target. At 16:9 every coordinate is exactly what the author wrote, so authors keep drawing at 1920×1080.

**Regions** are a fixed set defined by the format, so the player sees the same controls on every skin:

| Region | Holds | Player adjusts |
| --- | --- | --- |
| `playfield` | The lanes widget and every per-lane element, automatically | x, y, scale, lane width |
| `judge` | The judgment and combo widget, automatically; placed relative to `playfield` | x, y, scale, visible |
| `left`, `right`, `top`, `bottom` | HUD elements the author tags | x, y, scale, visible (when marked optional) |
| `screen` | Everything untagged, backgrounds (`fill = cover`, `stretch` or `none`) | nothing |

- Each region has a 9-point anchor; on a wider canvas it moves with its anchor. The player can also set a side inset that keeps edge regions near the centre on very wide screens.
- The transform order is: author coordinates → region scale about the anchor → region translation → anchor shift → canvas to target. `judge` is placed relative to `playfield`: it is scaled about its own anchor and translated by its own adjustment first, then moved by the playfield's translation and anchor shift, once, so the inherited move is never scaled; it does not take the playfield's scale or lane width. A test with the judge at a scale other than 100 % moves the playfield and checks the judge moves exactly as far. Adjustments are integers (reference pixels and percent) with bounds that keep a region on the canvas.
- **Lane width** scales lane-bound elements horizontally; nine-slice frames keep their borders.
- **Judgment-line height** is the `playfield` y: the whole field moves, its top included, so the visible time is unchanged. Lift, which shortens the lane and changes the visible range, stays a play setting (design.md "Algorithms").
- **Storage:** layout settings outside the skin, keyed by skin and aspect class (16:9, 21:9, 32:9, 16:10), so a skin update keeps them; a reset returns a region to the author's layout; they are device settings, one file per skin under the data root's `layouts\` (§7, decision 2).
- **Adjusting:** an adjust mode on the ready screen. A key or the turntable selects a region, arrows or the turntable move it, a modifier scales it, and region outlines are drawn with sprites, so no `egui` is needed. The values are also editable in the settings file.
- **Imported skins** sit in one `screen` region and are pillarboxed at other aspects, never stretched. They get no composition adjustment, since `screen` has no controls.

**Resize.** Today `prepare` gets the GPU and the chart and `build` gets no viewport (crates/render/src/skin.rs:14-34, 63-66). `prepare` gains the target's pixel size, from which it picks @2x images and rasterizes text at final size, and `FrameInput` gains the canvas size, so a resize during play recomputes anchors and the canvas fit without allocating. Assets chosen by pixel size stay as prepared until the next `prepare`, which runs between plays; a scale change in the adjust mode takes effect on screen at once and in the assets at the next `prepare`. Tests cover two pixel sizes of one aspect, and a resize during a prepared play followed by the next `prepare`.

**Rejected:** free per-element offsets (every skin would invent its own and players would face a different set per skin), constraint or flex layout (hard to learn, no export), per-element editing (players do not think in elements), non-uniform stretch (distorts).

### R6. Widgets

Built in, because they carry player logic or the scroll math, which stays in the player:
- `lanes`: notes, long-note head, active and inactive bodies, tail, mines, bar lines, the judgment line, and the lane cover. The scroll math is today's `layout::Scale` (crates/render/src/simple.rs:60-115), copied into `skin-model` until render switches over and then deleted from render.
- `judge`: grade images, the combo number placed relative to it, the shift that makes room for the combo.
- `gauge`: segments with images per gauge kind and state.
- `number`: a glyph strip of digits, with digit count, alignment and zero padding.
- `text`: chart-bound strings (title, artist and so on), rasterized at `prepare`, and literal strings.
- `bga`.

Composed by authors from elements, per-lane repetition and timers: key beams, bombs, key pads, progress (a `bar`), frames and backgrounds.

### R7. Images and scaling

Before play, never per frame, so per-frame cost is unchanged apart from trilinear sampling. Loading a skin (`Sheet::new`) checks its files and images and reads only their headers; the atlas (`Sheet::atlas`) decodes, packs and builds the mips for the chosen variant whenever that variant changes, between plays and off the in-play path, so a retry keeps it. This is how the loading thread and `prepare`'s pick of @2x (R5) fit together. A file whose dimensions changed since load is refused, and the player keeps its current skin.
- **Atlas pages** packed with `etagere`, sized within the limits the device was created with (`Device::limits()`; device creation requests defaults today, crates/render/src/gpu.rs:95), not the adapter's, so a skin that fits the adapter but not the device is refused at load rather than failing at texture creation. The pages are one texture array: every layer has one power-of-two side, from 256 (or the largest frame) to the device's side capped at 8 192, chosen as the fewest texels across all layers, reserved ones included, among the sides whose pages fit the device's layers and 64. The skin's pages start after the reserved layers (the bitmap font's layer 0), and a frame's uv is its content over the side, exact in `f32`.
- **Mipmaps** generated on the CPU, 4 levels, with premultiplied alpha (`(c·a + 127) / 255`) and a 2×2 box filter in stored (non-sRGB) space. Every frame of a grid and every glyph of a digit strip is packed on its own with an 8-texel gutter, its allocation aligned to 8, so every texel of every level covers one allocation and lower levels never mix neighbouring frames. The gutter and any rounding in the allocation repeat the frame's nearest edge texel, as clamp-to-edge would. Two images cutting the same rectangle share one packed frame. The tests use contrasting adjacent frames and colored transparent edges and sample them at every level.
- **Per-image filter:** linear by default, nearest for pixel art: nearest within a level and between levels, so shrinking still uses the box-filtered levels and never blurs two levels together.
- **Text** rasterized at `prepare` at its final pixel size with `cosmic-text` and swash. The play screen still never calls into `render::text`.
- **@2x variants** (S4): a file may name an @2x file at exactly twice its size. The atlas reads them when the canvas is magnified on both axes (`variant_for`), doubling the frames' rectangles; a file without one keeps its 1x frames, and track geometry does not change. Per-region picks come with regions (S5). The authored syntax for @2x, and nine-slice, come with the authored format (S12).
- **Elements and quads** (S4): an element's source is an image of the sheet; `Skin::build` takes the uv table of an atlas of the skin's sheet (a table of another sheet draws nothing) and writes the frame's uv and layer and the image's filter into the `Quad`. Neither breaks a run: blend and clip alone do (R1).
- **Bounds:** file, image and frame counts, name length, file size, header position, image dimensions and texels per file and per variant, atlas texels, and element, keyframe and timer counts are capped before anything that grows with them is allocated, and one file's decoding is bounded (design.md "Memory Budget"). Every referenced file resolves inside the skin's folder and must be a regular file, checked before opening and on the handle (design.md threat model "Skins", with its precondition and the decoder exception).

## 3. The beatoraja importer (`skin-beatoraja`)

- **What it reads:** JSON skins and Lua skins (`.luaskin`), whose returned table has the JSON shape. Load-time branching, includes and options are resolved at import; the result is a runtime model. Includes are bounded in depth and in total input bytes and expanded values, and a cycle is refused, header processing included; fixtures cover a self-include and a two-file cycle.
- **Lua** runs only on the loading thread, through `mlua` with vendored Lua 5.4, with the threat model's sandbox (design.md "Threat Model", "Lua skins"): an allowlist environment without `io`, `os`, `dofile`, `loadfile`, `debug` or unrestricted `package`, a module loader restricted to the skin's own folder, and caps on memory, instructions (coroutines included) and host calls. It covers the header pass (run without `skin_config`), the configured pass, the analysis of per-frame functions and the table conversion. An infinite loop, memory exhaustion and a runaway coroutine each end in a bounded refusal, and the player keeps the skin it had.
- **Per-frame functions.** Lua's per-frame closures are converted into expressions before the VM is discarded (design.md "beatoraja Lua Skins"). The supported subset is finite and listed: arithmetic and comparisons over `main_state.number`, `option`, `timer` and the timer utilities, module-local constants and helpers that inline to those, and the recognized observer patterns (`timer_observe_boolean` becomes a `when` timer). Anything else becomes a constant with a load warning when it is decorative. When an unsupported callback controls what the play depends on (the notes' drawing, the judgment display and its timers, the gauge, combo and score values), the skin is refused instead, because a frozen judgment timer or score number would load and look broken; a fixture keeps such an element present but frozen and expects the refusal.
- **The conversion report** tool runs the importer over a set of skins on the developer's PC (none are in the repository) and lists what did not convert. It decides which expressions and patterns to add.
- **Fixtures** in the repository are small self-made JSON and Lua skins. That a real skin plays is checked on the developer's PC.

## 4. The authored format (`skin-toml`)

**Concepts an author learns,** each with its payoff:

| Concept | What it gives the author |
| --- | --- |
| image | A file with optional grid frames, a frame cycle and its own timer (R2), as authors make sprite sheets today |
| element | One placement model for images, numbers, text, bars and widgets, in reference pixels |
| region | 21:9 support and player adjustment, by tagging HUD elements (the playfield and judgment are automatic) |
| expression | Conditions and values by name (`combo >= 100`) instead of numeric IDs |
| timer | What starts an animation: a built-in event or `when(<condition>)` |
| keyframes | Animation, with the semantics of R2 |
| option | Variants the player picks, folded at load |

`per_lane = true` repeats an element over the lanes with the lane's x and width; it is an attribute, not a concept. Cut: generic templates, includes, file choosers, free offsets, expressions that move or color things, subtract and multiply blends.

**Files.** `skin.toml` (TOML 1.1 through `toml` with `serde_spanned`) beside the images. `[meta]` (name, author, version, format version), `[options.*]`, `[images.*]`, then elements in draw order. A file of another format version is refused (AGENTS.md). Errors carry spans and are rendered with `annotate-snippets`: unknown names with a suggestion, type and range errors, and export warnings. KDL reads better for trees but has no maintained derive layer; YAML's implicit typing hurts hand authors.

**How the export always works:**
1. A construct enters the format only together with its export rule.
2. Expressions decide only visibility, numbers, text, frame index and bar ratio; geometry and color come from keyframes and regions, because beatoraja's keyframe geometry is plain integers and takes no Lua.
3. Expressions stay in a Lua-translatable subset (R4); division, remainder and conditionals become prelude functions (a conditional because `c and a or b` breaks when `a` is false).
4. Every state field states its beatoraja mapping or is tapestry-only. A decorative element that reads a tapestry-only field is left out of the export and listed in the report; `--strict` fails instead. The elements the play depends on (the lanes, judgment and gauge widgets, and the score and combo numbers) may depend only on exportable fields, in every option configuration: `check` refuses a skin that breaks this, so `build` never writes an export without notes. A fixture tests the refusal.

## 5. The beatoraja exporter (`skin-export`) and the CLI (`skin-cli`)

- **Target:** a generated `.luaskin` at exactly 1920×1080 (a test asserts the size: an unlisted size silently falls back to 1280×720 in both loaders), using only the fields and runtime API that upstream and ED share. Conditions are `draw` Lua strings, because ED's `op` takes integers only. The loaders compile each such string on its own in the skin's global environment, so the prelude helpers are installed as globals with a reserved prefix, not as locals of the generated file. A flag for upstream-only fields can come later. JSON was rejected: it cannot compute geometry and expands long.
- **Regions** become one user offset per region. The Lua reads the configured values at load (ED exports them before the configured pass, `LuaSkinLoader`, `SkinLuaAccessor`) and bakes each region's transform into its elements' keyframes; no destination offset applies them a second time. The built-in offsets are used only where their meaning matches: lift and lane cover for the covers. The whole-skin offset scales the lanes and BGA too, so it does not stand in for a HUD region.
- **Text:** literal text is baked into images; chart-bound text is exported as text with a font that may be redistributed and covers the needed glyphs, or left out with a report line. A title cannot be baked into a reusable skin.
- **Judgments and gauges** map by a table: tapestry's grades, empty POOR and the preset's gauges to beatoraja's grades and gauge types; a gauge with no counterpart maps to the nearest type with a report line.
- **What degrades:** aspect (16:9 only), the side inset, mipmaps and @2x (the 1x image), nine-slice (baked per size, edges rounded so seams do not open).
- **CLI `tapestry-skin`:** `check`; `preview`, which draws the compiled quads with `tiny-skia` into PNGs at 16:9 and 21:9 for a fixture state, so authors preview on macOS and Linux; `export beatoraja`; `build`, which does all three in one command; `watch`, which polls file times as preset files are polled (stage2.md D10). The preview lives in the CLI, so the player does not link it.
- **In the player** a changed skin is reloaded between plays, built off-thread; a failed reload keeps the current skin, and nothing is polled during play.

## 6. Crates, render, tests and PRs

### Crates

| Crate | Contents |
| --- | --- |
| `skin-model` | The runtime model, state schema, expressions, tracks, timers and reducer, regions, widgets, scroll math, atlas pages and mips, `Quad` and `QuadBatch` |
| `skin-beatoraja` | The JSON and Lua importer |
| `skin-toml` | The authored format, its compiler and diagnostics |
| `skin-export` | The beatoraja writer, from the published format only |
| `skin-cli` | The authoring CLI and its preview |

All are `GPL-3.0-or-later` (design.md "Licenses") and portable: each goes into `REQUIRED_PORTABLE`, the baseline workspace of `xtask/src/fixture.rs`, the license table of `check-arch`, and its in-play functions into `IN_PLAY`. A skin that tapestry exports is the author's work and data, not a derivative of tapestry.

### Render, after stage 2 exits

- `SpriteBatch` becomes one ordered list with runs broken at each blend or clip change; a pipeline per blend and a scissor per clip.
- `Sprite` grows from 48 to 64 bytes with an atlas layer and flags; the atlas becomes a texture array of one page side, the bitmap font in reserved layer 0 and the skin's pages after it (R7), uploaded with their mips as `Rgba8Unorm`, with a linear and a nearest sampler chosen per quad. `AtlasLimits` comes from `Device::limits()`. The uv table and the uploaded pages have one owner, and the memory budget counts the old atlas alive during a reload and the upload staging.
- Premultiplied alpha throughout: every pipeline, the bitmap font and `text_screen` (today `ALPHA_BLENDING`, crates/render/src/renderer.rs:799).
- The letterbox (renderer.rs:678) becomes the canvas fit, and `FrameInput` carries the canvas size.
- `PlaySkin::prepare` gains the play context and the target's pixel size; `DeclSkin: PlaySkin` uploads the atlas pages in `prepare` and delegates `build` to `skin-model`.

### Tests

**macOS and Linux:**
- Expressions: unit tests, range and bound errors, and a property test against a reference interpreter.
- Tracks: the endpoint table of R2.
- The reducer: effect scripts split across drains at every point, long-note feedback, `failed_at`, retry.
- `lanes`: `SimpleSkin`'s long-note, culling and density tests ported unchanged.
- Golden quad lists and preview PNGs at 16:9, 21:9 and 16:10 with a bundled test font, a resize of one prepared play, adjusted regions, an alpha/add/alpha overlap that fails if blend order is lost, and two same-blend elements with different clips followed by an unclipped one, checked by expected pixels.
- An allocation test around `build` on a dense chart, in dev and release (design.md "Scope of No Allocation").
- Importer: self-made fixtures, and the sandbox tests of §3.
- Export: both load passes, run under a Lua stub with and without `skin_config`; the generated properties executed against known states, checking values and visibility as well as geometry, with stated pixel and millisecond tolerances; the export size.
- A fuzz target for the skin loader.

**Windows:** the render integration, `render --test alloc` with `DeclSkin`, scripted plays with no render allocations, and the WARP render of a fixture against its CPU preview.

**On the developer's PC:** for every change to the format or the exporter, the standard skin's export loads and plays in pinned releases of upstream and ED (0.8.9 and 0.4.0 at the time of writing; the pins are bumped deliberately); for every change to the importer or to runtime semantics it relies on, the conversion report over the local skin set has no new failures (design.md "What Humans Verify").

### Pull requests

`[mac]` marks a PR that can start before stage 2 exits: it touches neither `render`, `player`, `game` nor `eval`.

| PR | Content | Start |
| --- | --- | --- |
| S1 | design.md and this file | [mac] |
| S2 | `skin-model`: schema, expressions, `Quad` types | [mac] |
| S3 | Tracks, timers and the reducer; the allocation test | [mac] |
| S4 | Images, atlas pages, mips; the @2x mechanism and its pick | [mac] |
| S5 | Canvas, regions, widgets with the scroll copy | [mac] |
| S6 | `skin-beatoraja`: JSON; enforces the element, `when` and key limits cumulatively while building, before compiling or allocating each item | [mac] |
| S7 | `skin-beatoraja`: Lua, sandbox, conversion, report tool | [mac] |
| S8 | `skin-cli`: `check`, `preview`, goldens | [mac] |
| S9 | Lane input in `PlayView`; fast and slow counts in `Tally` | coordinated with stage 2: it changes `game` and `eval`, and `PlayView` literals in render |
| S10 | Render integration; `SimpleSkin` stays the default | after stage 2 |
| S11 | Player: skin choice, layout settings, adjust mode | after stage 2 |
| S12 | `skin-toml`: the authored format; nine-slice and the @2x syntax; the same cumulative limits as S6 | [mac] |
| S13 | `skin-export` and `build` | [mac] |
| S14 | The standard skin; `SimpleSkin` removed | |
| S15 | BGA | |

S6 to S11 make existing beatoraja skins playable before S12 starts the authored format. The exit (design.md "Roadmap"): an existing beatoraja skin and the standard skin both play, and playing with the standard skin is fun.

## 7. Compromises and decisions

### What stage 3 ships

| Wish | Ideal | Stage 3 | Later |
| --- | --- | --- | --- |
| Other aspects | Reflow per aspect | Anchored regions, background fill, side inset; imports pillarboxed | Per-aspect overrides |
| Player layout | Drag anything | Seven fixed regions, lane width, key-driven adjust mode, numbers in the file; imported skins not adjustable | Mouse drag with `egui`; per-region opacity |
| Judgment-line height | Its own setting | `playfield` y; lift stays a play setting | Revisit if players ask |
| Enlarging | Vector everything | @2x, text at final size | SVG, SDF shapes, MSDF text |
| Shrinking | Perfect | Frame-aware mips, nearest for pixel art | — |
| Resize during play | Re-rasterize | Anchors and fit recomputed; assets refreshed between plays | — |
| Export | Everything | 16:9 1080p `.luaskin`, shared field set, regions as offsets, a report of what was left out | 16:10 and 4:3 bakes, an upstream flag; aspect, mips: never |
| Expressions | General | Typed, range-checked integers, Lua-translatable | Built-ins the conversion report asks for |
| Animation | Curves per key | Four eases per track | Per-key ease and in-out in the authored format, split into keys on export |
| Imports | All existing skins | beatoraja JSON and Lua | LR2 (decision 1) |

### Decisions

1. **LR2 skins.** (a) The LR2 importer right after stage 3. (b) Inside stage 3, also before the authored format. (c) Inside stage 3, after it. **Decision: (a).** beatoraja-family skins are what current players use, and the runtime model is built to grow with its first consumer, so the LR2 importer adds its own semantics (cubic and per-key ease, the LR2 IDs) when it comes.
2. **Where layout adjustments live.** (a) Device scope (follows the monitor and the seat). (b) Player scope. **Decision: (a),** keyed by skin and aspect class; the physical setup is what the adjustments depend on.
3. **Screens narrower than 16:9.** (a) The canvas grows taller. (b) Letterbox. **Decision: (a);** anchors already handle it.
4. **Scratch side.** (a) A player setting that the lanes widget lays out, exported as a one-of option. (b) Not in stage 3. **Decision: (a).**
5. **Name of the format and its file.** "tapestry skin", `skin.toml`.
6. **Fast/slow on the play screen.** (a) Add fast and slow counts to `Tally` in `eval`. (b) Leave them out. **Decision: (a);** it is pure counting in the evaluator.
