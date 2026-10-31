# Stage 3 Design: Skins

Stage 3 of the roadmap (design.md "Roadmap") is the look and effects of the play screen. This file plans the skin system: the runtime model the player draws from, the importers that make existing skins playable, tapestry's own skin format, and its export to beatoraja-family players.

**Status.** Decided; the decisions that change design.md are in it ("Goals", "Rendering and Skins", "Custom Skin Format", "beatoraja Lua Skins", "Extension Boundaries", "Settings Scopes", "Language and Crate Structure", "Libraries", "Roadmap", "Open Questions"). The fast and slow counts in `Tally` go into design.md with the evaluator change that adds them (S9). This file holds the plan and the details, and is removed when stage 3 exits.

**Citations.** `file:N` points into the repository at commit 17e17de unless it names another revision (8622dd7 for the canvas, regions and lanes of S5, c613b6f for the number, judgment and gauge widgets of S5b), and design.md is cited by section name. "ED" is LR2oraja Endless Dream; "upstream" is beatoraja itself. What is said about their skin loaders was read from their published source as a description of the file format; none of their code is ported (design.md "Libraries").

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
- Between keys, x, y, w, h, alpha, r, g, b and angle interpolate; a field left out of a key keeps the previous key's value. A key's `(x, y)` is the rect's top-left corner, as in the target loaders' destinations, so an importer maps them one to one. Each element names a 9-point pivot, a point of the sampled rect (corner, edge midpoint or centre), about which it rotates: with x = 100 and w from 40 to 80 the corner stays and a centre pivot moves from 120 to 140. The angle is in degrees and interpolates numerically (350 to 10 passes 180); drawing reduces it into `[0, 2π)` (`rem_euclid` before the conversion, so large keys keep their precision). `Quad` carries the pivot as a canvas point and the angle in radians.
- Ease per track: linear, in, out, step (in and out are quadratic). Cubic and per-keyframe ease come into the runtime model with the importer that needs them.
- **Image frames** are not part of the track: an image's frame cycle runs on the image's own timer, so an element can show on `ready` while its frames restart on `judge`. With `n` frames over a period `p` (1 µs to the longest key time), `t` after that timer started shows frame `((t mod p) · n) / p`; frame 0 shows while the timer is off, without a cycle, or with one frame. Both timers come from the same frame's fill, before any element is drawn. Whether the target loaders show frame 0 while a cycle's timer is off is checked when S6b imports; the importer adapts, the rule stays.
- In the authored format, keys must be strictly increasing from `k0 ≥ 0` and a loop point must lie inside the track (the target loaders hide a finished track by setting its time to −1, which a negative key would make visible again); anything else is a load error. The target loaders accept unsorted and equal-time keys and apply a loop only when the last key is after 0; the importer reproduces that, and the exporter emits only tracks that need neither. Tests pin `k0`, `kn` and `L` at exactly and ±1 µs, for hold, hide, loop and a single-key track.

### R3. State and timers

The skin reads a per-frame `SkinFrame`, filled from the render thread's `FrameInput`, the chart, a play context given to `prepare`, and a reducer over the drained effects. The play context is fixed for one attempt: the preset's gauge descriptors (`start`, `max`, the gauge's `look` and its `border`, in the order of `PlayView::gauges`, which carry values only, crates/game/src/render.rs:17-28), the attempt's start position and, in practice, its section, so the lanes widget draws only the section's targets (stage2.md D15) and `ready` and `play` start from the attempt's own countdown, not chart time 0.

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

**Canvas.** The canvas is 1080 reference pixels tall and `1080 × aspect` wide; narrower than 16:9 it is 1920 wide and grows taller instead (§7, decision 3). The aspect is capped at 4:1 (4320 wide) and 1:2 (3840 tall), with bars beyond; the branch is decided in integers, so a target just narrower than 16:9 grows taller, never narrower. It is scaled uniformly to the render target. At 16:9 every coordinate is exactly what the author wrote, so authors keep drawing at 1920×1080. `Skin::canvas` computes it from the target size; a skin with `Fit::Fixed` (imports) uses 1920×1080 pillarboxed or letterboxed, as render's letterbox does today (crates/render/src/renderer.rs:681 at 8622dd7).

**Regions** are a fixed set defined by the format, so the player sees the same controls on every skin:

| Region | Holds | Player adjusts |
| --- | --- | --- |
| `playfield` | The lanes widget and every per-lane element, automatically | x, y, scale, lane width |
| `judge` | The judgment and combo widget, automatically; placed relative to `playfield` | x, y, scale, visible |
| `left`, `right`, `top`, `bottom` | HUD elements the author tags | x, y, scale, visible (when marked optional) |
| `screen` | Everything untagged, backgrounds (`fill = cover`, `stretch` or `none`) | nothing |

- Each region has a 9-point anchor; on a wider or taller canvas it shifts by `(i + fx·(cw − 2i − 1920), fy·(ch − 1080))` for anchor fractions `fx`, `fy` and side inset `i`. The player's inset keeps edge regions near the centre on very wide screens; it is clamped to `max(0, (cw − 1920) / 2)` each frame and never moves `screen`.
- A region's box is the union of its elements' key rects (unrotated), computed at load; the point of its anchor in that box is its scale pivot.
- The transform order is: author coordinates → lane width (playfield, horizontal) → region scale about the anchor point → region translation → anchor shift → canvas to target. A `fill` background on `screen` (`cover` or `stretch`) skips the regions and fills the canvas. `judge` is placed relative to `playfield`: it is scaled about its own anchor and translated by its own adjustment first, then moved by the playfield's translation and anchor shift, once, so the inherited move is never scaled; it does not take the playfield's scale or lane width. A test with the judge at a scale other than 100 % moves the playfield and checks the judge moves exactly as far.
- **Ranges and the clamp.** Adjustments are integers: offsets within ±4320 reference pixels, scale and lane width 50–200 %, inset ≤ 1200; `Layout` refuses anything else. Each frame the translation is clamped per axis so that the region's logical box (its box after lane width, scale, translation and anchor shift) overlaps the canvas by at least `min(extent, 64)` pixels. `judge` and `playfield` are clamped jointly: with `P` the playfield's range and `J` the judge's (its inherited anchor shift included), the judge's own offset is clamped to `[Jlo − Phi, Jhi − Plo]`, then the playfield's to `P ∩ (J − local)`, so the judge still moves exactly as far as the playfield. A range whose author placement overlaps less keeps 0. The clamp does not promise that rotated geometry or a pivot stays on screen; `visible` is the control for hiding.
- **Controls.** `Skin::controls` says what the skin offers per region (this table; lane width only with the lanes in `playfield`, none for a region without elements or under `Fit::Fixed`); the layout's value for a control the skin does not offer is ignored.
- **Lane width** scales lane-bound elements horizontally about the playfield's anchor; nine-slice frames keep their borders.
- **Judgment-line height** is the `playfield` y: the whole field moves, its top included, so the visible time is unchanged. Lift, which shortens the lane and changes the visible range, stays a play setting (design.md "Algorithms").
- **Storage:** layout settings outside the skin, keyed by skin and aspect class (16:9, 21:9, 32:9, 16:10), so a skin update keeps them; a reset returns a region to the author's layout; they are device settings, one file per skin under the data root's `layouts\` (§7, decision 2).
- **Adjusting:** an adjust mode on the ready screen. A key or the turntable selects a region, arrows or the turntable move it, a modifier scales it, and region outlines are drawn with sprites, so no `egui` is needed. The values are also editable in the settings file.
- **Imported skins** use `Fit::Fixed`: one `screen` region, pillarboxed at other aspects, never stretched. They get no composition adjustment, since `screen` has no controls.

**Resize.** Today `prepare` gets the GPU and the chart and `build` gets no viewport (crates/render/src/skin.rs:14-34, 63-66). `prepare` gains the target's pixel size, from which it picks @2x images and rasterizes text at final size, and `FrameInput` gains the target size, from which the skin computes the canvas each frame (`SkinFrame::target`), so a resize during play recomputes anchors and the canvas fit without allocating. Assets chosen by pixel size stay as prepared until the next `prepare`, which runs between plays; a scale change in the adjust mode takes effect on screen at once and in the assets at the next `prepare`. Tests cover two pixel sizes of one aspect, and a resize during a prepared play followed by the next `prepare`.

**Rejected:** free per-element offsets (every skin would invent its own and players would face a different set per skin), constraint or flex layout (hard to learn, no export), per-element editing (players do not think in elements), non-uniform stretch (distorts).

### R6. Widgets

Built in, because they carry player logic or the scroll math, which stays in the player:
- `lanes` (S5): notes, long-note head, active and inactive bodies, tail, mines, bar lines, the judgment line, the lane cover and the lift cover, one image per part and lane.
  - **Geometry.** Explicit lane rects `[x, w]` with an authored scratch side, the scratch outside every key. On the other side, the scratch moves to `span_start + span_end − x − w` and the key block to the near end, each key keeping its offset inside the block. Elements bound to a lane (`Bind::Lane`) or to the span (`Bind::Lanes`) map their x and w from the authored lane to the current one; `Skin::placement` takes the scratch side, which `build` reads from the attempt (`PlayContext::scratch`).
  - **Cover and lift** in ‰ of the lane length, lift at most 900 and cover at most `900 − lift`; the scroll span is the uncovered length and the lift raises the judgment line.
  - **Section targets.** In a practice round only the section's targets are drawn (notes from its start, ending before its end, as the evaluator decides), and once evaluation is complete the round rings out without notes (`SkinFrame::complete`).
  - **Budget.** `LANE_QUADS` (8 192) quads for notes and bar lines plus 3 reserved for the line, the cover and the lift, and `LANE_VISITS` (16 384) candidates visited, charged before any skip (judged, outside the section, culled); running out stops the frame's lanes with one drop.
  - **Cropping.** A part crossing the lanes' top or bottom is cut with its uv, never squashed.
  - The lanes' track, like the judgment display's, gives its color and alpha only; geometry or an angle is a load error. Number and gauge elements are placed by their track like images.
  - The scroll math is today's `layout::Scale` (crates/render/src/simple.rs:60-115), copied into `skin-model` parameterized by the span and the line, until render switches over and then deleted from render; with no cover or lift every output bit equals render's, and a test reads render's source to guard the copy.
- `number` (S5b): a glyph strip (`DigitsDef`) showing an `int` expression over the play schema, charged to the op budget once per element that draws it.
  - **Glyphs.** `Ten` (digits), `Eleven` (a padding glyph 10) or `TwentyFour` (12 glyphs for values at or above 0, then 12 for values below, each half with the padding glyph 10 and a sign 11). An image of `k` times as many frames holds `k` glyph sets, cycled over the sets on the image's own timer; set 0 shows while that timer is off.
  - **Cells.** 1 to `MAX_CELLS` (11: the 10 digits of an `i32` and a sign). Cells fill from the right with the digits of `|v|` (`unsigned_abs`, so `i32::MIN` shows every digit); a value longer than the strip keeps its low digits. Left of the digits, the padding: zero, glyph 10, or blank. With `TwentyFour`, zero or glyph padding gives cell 0 to the sign, and blank padding puts the sign in the first blank left of the digits, so a value filling every cell shows no sign; without `TwentyFour` a negative value shows its magnitude.
  - **Geometry.** The track's rect is cell 0; cell `j` is at `x + p·j − s` with the pitch `p = w + space` (space signed, within ±2^20) and `s` 0, `p·b` or `p·b/2` for right, left or centre alignment over `b` blank cells. Each cell turns about its own 9-point pivot. A bound number maps each cell's x and w through the lane map, so the spacing follows the lane width. Its logical box is, per key, cell 0's rect with cell `n − 1`'s.
- `judge` (S5b): one grade image per slot (`JudgeSlot`: PERFECT, GREAT, GOOD, BAD, POOR, empty POOR, PERFECT at a full gauge) and one optional combo strip per slot, each part with its own track, timer, 9-point pivot, blend and visibility.
  - **What it shows.** The reducer's latest judgment's slot, nothing before the first one. PERFECT while `gauge_max` is on uses the full-gauge slot; its grade image and its combo each fall back to PERFECT's on their own. The combo shows the snapshot's `combo` field, only when it is above 0, the slot has a combo, and the combo part's timer is on and its visibility true; it reads no value from the effect, which may be behind the snapshot (R3). Nothing is drawn while the grade part is not.
  - **Placement.** The combo's track is relative to the grade's sampled rect: x from its left edge, y from its top or bottom edge (`Attach`). With `shift`, the grade moves left by half the combo's drawn width `p·(cells − blanks)`. The combo's cells are pushed first, then the grade, each under its part's blend and the element's clip, so a judgment display is at most two runs. The element's own track multiplies its color and alpha into both parts.
  - **Region.** In `judge` (in `screen` under `Fit::Fixed`) and never bound to the lanes. Its logical box is the union over the effective states (each slot, and the full-gauge PERFECT with its fallbacks) of the grade's keys and the combo's strip boxes moved over the grade corner's range on the attach edge, with the grade's box widened on both sides by half the widest strip when `shift` is on. Every part's keys count toward the skin's key limit, and part visibilities are charged per element.
- `gauge` (S5b): the shown gauge (`SkinFrame::gauge`) as segments of the track's rect.
  - **Looks.** Front, back and tip images, each at or above and below the border, for each look (`GaugeLook`: assist easy, easy, normal, hard, ex hard, hazard). The descriptor's look comes from the preset gauge's id, or for another id from its kind (an end-threshold gauge is normal, a survival gauge hard); its border is the clear threshold or the value a survival gauge fails below.
  - **Segments.** `P` segments (1 to `MAX_SEGMENTS`, 1 000). With the value `V` and the maximum `M`, `n = 0` lit segments when `V ≤ 0`, else `⌊V·P/M⌋` within 1 to `P`, so any non-zero gauge lights one; segment `i` (from 1) is below the border when `i·M < border·P`. `MAX_SEGMENTS` is a policy cap, one segment per ‰: the widget reads the gauge's exact value, not the ‰ field.
  - **Motions,** pure functions of the element's timer, with `k = ⌊t / step⌋` and the offset `a`: still (`a = 0`), random (`a` = splitmix64 of `k` mod `range + 1`), rise (`a = −k mod (range + 1)`), fall (`a = k mod (range + 1)`). Segment `n` is the tip, below it front while `i + a < n`, the rest back. Flicker draws front up to `n` and back after it, and overlays the tip on segment `n` with alpha `1 − |2φ − T|/T` at the phase `φ` of the period `T`. These differ from the target: its flicker curve differs (1.032 against 0.970 at 16 ms of a 33 ms period), its rise, fall and random advance once per drawn frame rather than by elapsed time, so their phase and catch-up after a frame gap differ, and its random sequence differs.
  - Each segment turns about its own 9-point pivot, and a bound gauge maps x and w through the lane map.
- `text` (S5c): one line (`TextDef`) in a font file of the sheet (`FontRef::File`) or the player's text font (`FontRef::Default`: the menu font in the player, the test font in the preview and goldens).
  - **Strings.** A literal of 1 to 256 bytes with no control character (C0, DEL or C1), or a `TextField`: title, subtitle, artist, subartist, genre, and the full title and full artist, joined with a space when the second part is not empty (refs 12 and 16, the most used). The caller passes `ChartText` filled from one record, the chart's static top-level header (what `bms_parser::read_header` reads and the library stores, as the select screen shows it), for display only. Headers inside `#IF`, `#SWITCH` or `#SETSWITCH` blocks are ignored, overrides of a top-level value included, and a field with no top-level declaration is empty. Table, target, rival and player names do not exist in tapestry; string expressions and first-non-empty fallbacks are not in the model.
  - **Requests.** `Skin::text_requests(&ChartText, target, &Layout)` runs at `prepare` and may allocate. Per text element (its slot is its place among the text elements in draw order) whose visibility does not fold to false: the string, control characters dropped after the full fields are joined, cut to 256 bytes at a character boundary; an empty one gives no request. The em is `min(largest key h × canvas scale × the region's effective scale, 256)` pixels, from the exact fractions `variant` uses (`Fit::Fixed` ignores layout adjustments). An element in a hidden region still asks, since the adjust mode can show it.
  - **Table.** The rasterizer (S8) answers a request with a `TextImage`: its uv and layer, the ink rect and the pen advance, in em. `TextImage::new` derives the line's extent, `left = min(0, ink.x)` and `width = max(advance, ink.x + ink.w) − left`, and refuses a non-finite value, ink 0 wide or tall, a negative advance, and any value or extent past ±4 096 em. `Skin::attach_text` checks the table's length against the text slots and gives it to the `Play`, which every `prepare` makes without one, so a table never outlives its attempt.
  - **Layout.** The track's rect `[x, y, w, h]` is the box: `h` is the em, and the box's top is the font's cap height, which the rasterizer folds into the ink rect. With `tw = width·h`, `Shrink` squeezes horizontally to `sx = w / tw` only when `tw > w`, else `sx = 1`, so it never enlarges; the drawn width `d = tw·sx` sits at offset 0, `(w − d)/2` or `w − d` for left, centre and right, and `Spill` overflows (centred text both ways). The quad is `[x + off + (ink.x − left)·h·sx, y + ink.y·h, ink.w·h·sx, ink.h·h]`, turned about the element's 9-point pivot on the box, sampled linearly, with the track's color and alpha over white premultiplied coverage. A line drawn 0 wide, a slot without an image and a table of another length draw nothing. The metrics are in em, so the raster size changes only sharpness, and an animated `h` rescales the line uniformly. The text stays as prepared through a resize (R5).
  - **Load checks** in `Skin::new`'s order: more than 64 text elements is `TooManyTexts`; `UnknownWidget` covers a missing `TextDef`; a font index past the sheet's fonts is `UnknownFont`; a text element bound to the lanes is `Bound`, one with a fill `Fill`, and one with a key of `h ≤ 0` or `w < 0` `Size`; a bad literal is `Literal`. Text charges no ops beyond its visibility. Its region box is its keys' rects: spilled ink does not count. `Skin::variant` skips text, which has its own pixel size.
  - **Fonts.** `Sheet::with_fonts` takes at most 16 font files, each name at most 1 024 bytes, resolved inside the skin folder to a regular file of at most 32 MiB, 64 MiB in all, whose first 4 bytes are `00 01 00 00`, `OTTO`, `true` or `ttcf`; `skin-model` parses none of them. Bitmap `.fnt` fonts are not accepted (§3).
  - Not in the model: wrapping, truncation, outline, shadow and editable text; the surveyed play skins use none of them.
- **Budgets.** At most `cells` quads for a number, `segments` (one more with flicker) for a gauge, the most over a judgment display's effective states of 1 plus the resolved combo's cells, and 1 for a text. The skin's widget quads are capped at `MAX_WIDGET_QUADS` (16 384, the element limit). `quad_capacity` adds them to the images' and the lanes', so the worst batch is 40 963 entries: about 2.5 MiB of quads and 0.625 MiB of runs (16 B each), about 3.125 MiB without container overhead.
- Frame by expression (an image's frame chosen by a value) comes with S6c, driven by the JSON import.
- `bga`.

Composed by authors from elements, per-lane repetition and timers: key beams, bombs, key pads, progress (a `bar`), frames and backgrounds.

### R7. Images and scaling

Before play, never per frame, so per-frame cost is unchanged apart from trilinear sampling. Loading a skin (`Sheet::new`) checks its files and images and reads only their headers; the atlas (`Sheet::atlas`) decodes, packs and builds the mips for the chosen variant whenever that variant changes, between plays and off the in-play path, so a retry keeps it. This is how the loading thread and `prepare`'s pick of @2x (R5) fit together. A file whose dimensions changed since load is refused, and the player keeps its current skin.
- **Atlas pages** packed with `etagere`, sized within the limits the device was created with (`Device::limits()`; device creation requests defaults today, crates/render/src/gpu.rs:95), not the adapter's, so a skin that fits the adapter but not the device is refused at load rather than failing at texture creation. The pages are one texture array: every layer has one power-of-two side, from 256 (or the largest frame) to the device's side capped at 8 192, chosen as the fewest texels across all layers, reserved ones included, among the sides whose pages fit the device's layers and 64. The skin's pages start after the reserved layers (the bitmap font's layer 0), and a frame's uv is its content over the side, exact in `f32`.
- **Mipmaps** generated on the CPU, 4 levels, with premultiplied alpha (`(c·a + 127) / 255`) and a 2×2 box filter in stored (non-sRGB) space. Every frame of a grid and every glyph of a digit strip is packed on its own with an 8-texel gutter, its allocation aligned to 8, so every texel of every level covers one allocation and lower levels never mix neighbouring frames. The gutter and any rounding in the allocation repeat the frame's nearest edge texel, as clamp-to-edge would. Two images cutting the same rectangle share one packed frame. The tests use contrasting adjacent frames and colored transparent edges and sample them at every level.
- **Per-image filter:** linear by default, nearest for pixel art: nearest within a level and between levels, so shrinking still uses the box-filtered levels and never blurs two levels together.
- **Text** (S8; uploaded in S10) rasterized at `prepare` at its final pixel size: `cosmic-text` lays the line out and swash renders white premultiplied coverage, with the features `std` and `swash` and fonts passed as bytes, never memory-mapped or found through fontconfig; a bounded `Sheet::font_data(i)` reads them and checks the path again, as atlas builds do. A glyph the skin's font lacks comes from the default font, then its `.notdef`. The play screen still never calls into `render::text`.
  - **Collections.** Only a validated face 0 is registered (as the target's FreeType generator uses face 0). No bulk loader that enumerates or reserves by a collection's declared face count is called (fontdb 0.23's `load_font_data` reserves `count` IDs before validating); if a bulk API is used, the `ttcf` face count is read and bounded first. A 12-byte `ttcf` header with count `0xffffffff` is refused by a test.
  - **Cap height,** the box's top: a positive `sCapHeight`, else the positive top above the baseline of the same face's own `H` glyph (no glyph fallback), else a positive ascender; zero and absent values both fall through. Tests cover a zero and an absent `sCapHeight`.
  - **Text layer.** The lines are packed into one layer after the skin's pages, reserved in `Sheet::atlas`'s layer count and texel budget when the skin has text, its side at least 2 048, with `etagere`, the existing gutter, alignment and mips. A line is at most `min(4096, side − 2·GUTTER)` pixels wide; a wider one is set at a smaller em. When the layer is full, every request is set again at half the em cap, at most 8 rounds.
  - **Raster bounds.** Each image's pixel dimensions and temporary raster bytes are bounded before anything is allocated, since a font's units per em and glyph bounds can make one glyph thousands of em tall. A request that still does not fit after the refit rounds gives no image for its slot (nothing drawn, one report line), not an error for the whole `prepare`.
- **@2x variants** (S4): a file may name an @2x file at exactly twice its size. `Skin::variant` picks them when some image the skin can draw is magnified on both axes: per element the canvas scale times its region's scale (hidden regions skipped), a `cover` or `stretch` fill's own scale on `screen`, and for lane-bound images and the lanes `min(lane width, 100 %)` on x, so a lane width alone never picks @2x; exact rationals, one variant for the sheet. The atlas then doubles the frames' rectangles; a file without one keeps its 1x frames, and track geometry does not change. A scale change in the adjust mode reaches the assets at the next `prepare`. The authored syntax for @2x, and nine-slice, come with the authored format (S12).
- **Elements and quads** (S4): an element's source is an image of the sheet; `Skin::build` takes the uv table of an atlas of the skin's sheet (a table of another sheet draws nothing) and writes the frame's uv and layer and the image's filter into the `Quad`. Neither breaks a run: blend and clip alone do (R1).
- **Bounds:** file, image and frame counts, name length, file size, header position, image dimensions and texels per file and per variant, atlas texels, and element, keyframe and timer counts are capped before anything that grows with them is allocated, and one file's decoding is bounded (design.md "Memory Budget"). Every referenced file resolves inside the skin's folder and must be a regular file, checked before opening and on the handle (design.md threat model "Skins", with its precondition and the decoder exception).

## 3. The beatoraja importer (`skin-beatoraja`)

- **What it reads:** JSON skins and Lua skins (`.luaskin`), whose returned table has the JSON shape. Load-time branching, includes and options are resolved at import; the result is a runtime model. Includes are bounded in depth and in total input bytes and expanded values, and a cycle is refused, header processing included; fixtures cover a self-include and a two-file cycle.
- **The JSON reader** (S6a) is our own and follows the source's lenient reader (libGDX 1.9.9 grammar, pinned). `//` and `/* */` comments and trailing commas are accepted, and a line break separates members or items where a comma is missing (one inside a block comment does not; two values on one line with no comma are refused). An unquoted name ends at `:`, a line break or a comment, an unquoted value at `,`, `}`, `]`, a line break or a comment; both keep inner spaces and drop trailing whitespace. In an object only `}` closes, so a name may start with `]`; a value cannot start with `}`. Only an unquoted token is a number, `true`, `false` or `null`. Members stay in order, duplicates included, and numbers and strings keep their spelling until a typed read converts them. The nodes live in one flat arena of spans into the input (28 bytes a node), and strings are decoded only when read. Intentional policies: a UTF-8 BOM is skipped; invalid UTF-8, an invalid escape and a lone surrogate (`\u` takes four hex digits) are syntax errors with file, line and column, found by the first scan wherever they are; a number read as an integer truncates toward zero and must fit `i32`. The caps are design.md's "Threat Model", each charged before its work.
- **Resolution** (S6a) expands `include` and `if`/`value`/`values` for one set of enabled options, field by field, by the type the field expects, and only in the types the source registers wrappers for: in the header, `property`, `filepath` and `offset` and their arrays. Everything else (option items, `category`, and until S6b the fields it maps) is read literally or not visited, and nothing it names is read. In an array field, a conditional item adds its `value` and then its `values` when its condition holds, an `include` item splices the included array, and any other item is one record; a record held in an array is a first-match choice. A condition is a number (`n` enabled, `-n` not enabled), or an array whose items all hold, an inner array holding when any of its numbers does. An unselected branch's include is never read. Wrapper keys match ignoring ASCII case and the first wins; ordinary fields match exactly and the last wins. An include resolves from the folder of the file naming it, asset paths from the entry file's folder. Intentional policies: a missing include is an error, an include name with `*` is refused, and a record that is not an object reads as the defaults.
- **The header** (S6a): `read_header` resolves with no option enabled, as the source's header pass does, and keeps the parsed definition, so `resolve` runs again with the configured options. `type` must be 0 (7-key play). The size is 1280×720, 1600×900, 1920×1080, 2560×1440 or 3840×2160, 1280×720 when `w` and `h` are omitted; any other is refused. It gives the name, author, options (items with their numbers, the default), file choices and offsets (id and axis flags), each list capped (1 024 options, 16 384 items, 1 024 file choices, 1 024 offsets) before each record is added. The offsets are only those the definition declares: the source also offers four built-in play offsets, which need a home before S11's adjust mode. A file choice's path is a folder and a pattern with `*` in its last component only, the whole name checked as an include's is before it is split; it resolves from the entry file's folder, wherever it is declared. The pattern matches ignoring ASCII case, as the source's glob does on its target's case-insensitive file system (an intentional subset: other letters are not folded); `|`, `?`, `[`, `]`, `{` and `}` are refused, a missing folder gives no candidates, and the candidates are sorted by name.
- **Mapping** (S6b) must:
  - Reflect the source's bottom-left coordinates: `top′ = rnd((H − y − h)·r)`, `bottom′ = rnd((H − y)·r)`, with pivots reflected and angles and vertical offsets negated; lanes take `top′ = (H − y − h)·r` and `line′ = (H − y)·r`. R2's statement of the source's axes follows this, and a test covers an asymmetric rectangle with a non-central pivot and a rotation.
  - Apply one critical or decorative policy through dependency traversal: unsupported behaviour that affects critical geometry, visibility, timing or a value refuses the skin (S6c's features included until they land), and the approved cosmetic approximations are listed. An unknown numeric op hides the element for either sign; a critical unknown refuses.
  - Lanes: a note's height from `size[i]`, else the note frame's height scaled; `lnbody` while active and `lnactive` otherwise, the `lnbodyActive` form, `dst2`, and which `group` subset maps to the bar part.
  - Tracks: inherit in source order with a stable sort, and normalise only cases proven equivalent (a loop past the last key falls back to the first key); tests sample values, including equal-time keys (discontinuity and loop length) and a negative start under quadratic easing. A stretch mode is baked only if it commutes with interpolation.
  - Rounding: a widget-specific bound or a pitch-preserving representation, tested at the last digit's edge and on the judgment and combo alignment.
  - Offsets: ids deduplicated, axis masks, relative judgment numbers, alpha.
  - Images: `len` is a count of selector groups (S6c, or unsupported until then); an omitted `w` or `h` is 0 and −1 is the texture size; a non-positive division is 1.
  - Budget: one budget across passes, charged per referencing element, with expression source built within bounds, the filter variants' frames, the judgment parts' keys and text quads counted, the maximum taken over the judgment display's states, and the case where compiling precedes the op charge settled explicitly.
  - Text: `ref` takes precedence over `constantText`; the "1–2 px" of metric differences is not a bound. Gauge differences are reported; a centre of −1, 10 or 11 is handled; a decorative asset's failure follows the stated policy.
  - Tests: witnesses around compiling, opening and construction; the generic timer budget at its boundaries; deduplication on real ids; every successful generated import passes all of `Skin::new`; a synthetic `TextTable` end to end; fuzz seeds backed by resources.
- **Lua** runs only on the loading thread, through `mlua` with vendored Lua 5.4, with the threat model's sandbox (design.md "Threat Model", "Lua skins"): an allowlist environment without `io`, `os`, `dofile`, `loadfile`, `debug` or unrestricted `package`, a module loader restricted to the skin's own folder, and caps on memory, instructions (coroutines included) and host calls. It covers the header pass (run without `skin_config`), the configured pass, the analysis of per-frame functions and the table conversion. An infinite loop, memory exhaustion and a runaway coroutine each end in a bounded refusal, and the player keeps the skin it had.
- **Per-frame functions.** Lua's per-frame closures are converted into expressions before the VM is discarded (design.md "beatoraja Lua Skins"). The supported subset is finite and listed: arithmetic and comparisons over `main_state.number`, `option`, `timer` and the timer utilities, module-local constants and helpers that inline to those, and the recognized observer patterns (`timer_observe_boolean` becomes a `when` timer). Anything else becomes a constant with a load warning when it is decorative. When an unsupported callback controls what the play depends on (the notes' drawing, the judgment display and its timers, the gauge, combo and score values), the skin is refused instead, because a frozen judgment timer or score number would load and look broken; a fixture keeps such an element present but frozen and expects the refusal.
- **Widgets** map onto R6's fields:
  - `value` becomes a `number`: the glyph layout from the frame count (a multiple of 24 is `TwentyFour`, else a multiple of 10 `Ten`, else `Eleven`, which forces glyph padding), a `digit` above 11 refused or, blank-padded and right-aligned, shifted by the extra cells. The target draws nothing for `i32::MIN` and `i32::MAX`, so the importer adds `v != MIN && v != MAX` to the element's visibility, charged as ops.
  - `judge` becomes a judgment display in `screen`: the outer destination's timer, conditions and lifetime go onto the element, which gets a neutral color track (white, opaque), since the target does not multiply that destination's color into its children. Each combo becomes `Attach::Bottom` with `cy = −(combo y + combo h)` in top-left terms, so no keys are rewritten across independently timed tracks.
  - `gauge` takes its parts as static frames (the target builds them with timer and cycle 0) and ignores the destination's angle (the target draws segments unrotated).
  - `text` becomes a `text`. `ref` 10 to 16 map to the fields (12 and 16 to the full ones) and `constantText` to a literal; an empty constant is dropped. The box is the destination moved left by `w·{0, ½, 1}` for left, centre and right. `overflow` 0 is `Spill`, 1 `Shrink`, and 2 `Shrink` with a report line. A TrueType font's `size` is ignored (the em is the destination's `h`). Outline, shadow, wrapping and editable are dropped with a report line, as are the other refs (1003, 1001, 1002, 3, 1, 2). `value` functions go to S7: a plain `main_state.text(n)` converts, and a fallback for an empty string waits on S7's decision. Both formats put the cap height at the box's top, so the destination maps with no vertical correction; that removes only the fixed vertical offset, since the target extracts metrics differently (its bearing cancellation, its cap probe glyphs), and S6b reproduces or reports the difference. Bitmap `.fnt` fonts (19 of the 38 surveyed text definitions) need a decision here: a bitmap-font text source, or the TrueType or default font substituted with a report line. LR2 skins, after stage 3: `#SRC_TEXT` gives the font, string id and alignment, and `#FONT` and `#LR2FONT` the fonts.
  - Frame by expression (`imageset` with a value), per-digit offsets (`value.offset`) and the schema fields the conversion report asks for (BPM, rate, remaining time, covers, gauge kind) come with S6c, with their importer mappings.
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
- **Text:** literal text is baked into images at 1x. Chart-bound text exports as `text` plus `font`, copying the author's font file, which must be redistributable and cover the needed glyphs: `size` is the largest key `h`, `Shrink` is `overflow` 1 and `Spill` 0, and the destination x moves right by `w·{0, ½, 1}` for left, centre and right. Chart-bound text in the default font cannot ship a redistributable font, so it is left out with a report line. A title cannot be baked into a reusable skin.
- **Judgments and gauges** map by a table: tapestry's grades, empty POOR and the preset's gauges to beatoraja's grades and gauge types; a gauge with no counterpart maps to the nearest type with a report line.
- **Widgets** export as the target's `value`, `judge` and `gauge`, and what differs is reported:
  - Numbers: a `Ten` image whose frame count is a multiple of 24 is rewritten, since the target tries 24 first: a cycled strip is split, keeping its set boundaries and wrap, and padding is allowed only for one set or no cycle. `Eleven` with blank or zero padding is reported (the target forces glyph padding) or rewritten to `Ten`. The target hides `i32::MIN` and `i32::MAX`, which is reported.
  - Judgment displays: a `TwentyFour` combo is not exportable (the target's judge takes 10 or 11 glyphs) and is reported. A `Bottom` combo exports directly; a `Top` combo exports only where, per effective state (each slot and the full-gauge PERFECT, grade and combo resolved), every grade key has one height, giving `combo y = grade h − cy − combo h` per key; when a fallback-shared `Top` combo needs different offsets in different states, separate combo entries are written (grade 0 of height 80 and grade 6 of height 100 sharing a `Top` combo at y 10, h 20 give offsets 50 and 70). Otherwise it is reported as not representable.
  - Gauges: a part with an animated image or a non-zero angle is reported (or baked to frame 0 and angle 0 with a report line). Each motion exports as the target type of its name, `Still` as random with range 0, and the report states the motion differences (R6).
- **What degrades:** aspect (16:9 only), the side inset, mipmaps and @2x (the 1x image), nine-slice (baked per size, edges rounded so seams do not open).
- **CLI `tapestry-skin`:** `check`; `preview`, which draws the compiled quads with `tiny-skia` into PNGs at 16:9 and 21:9 for a fixture state, so authors preview on macOS and Linux; `export beatoraja`; `build`, which does all three in one command; `watch`, which polls file times as preset files are polled (stage2.md D10). The preview lives in the CLI, so the player does not link it.
- **In the player** a changed skin is reloaded between plays, built off-thread; a failed reload keeps the current skin, and nothing is polled during play.

## 6. Crates, render, tests and PRs

### Crates

| Crate | Contents |
| --- | --- |
| `skin-model` | The runtime model, state schema, expressions, tracks, timers and reducer, regions, widgets, text requests and font files, scroll math, atlas pages and mips, `Quad` and `QuadBatch` |
| `skin-beatoraja` | The JSON and Lua importer |
| `skin-toml` | The authored format, its compiler and diagnostics |
| `skin-export` | The beatoraja writer, from the published format only |
| `skin-cli` | The authoring CLI and its preview |

All are `GPL-3.0-or-later` (design.md "Licenses") and portable: each goes into `REQUIRED_PORTABLE`, the baseline workspace of `xtask/src/fixture.rs`, the license table of `check-arch`, and its in-play functions into `IN_PLAY`. A skin that tapestry exports is the author's work and data, not a derivative of tapestry.

### Render, after stage 2 exits

- `SpriteBatch` becomes one ordered list with runs broken at each blend or clip change; a pipeline per blend and a scissor per clip.
- `Sprite` grows from 48 to 64 bytes with an atlas layer and flags, and carries the quad's angle and pivot, still 64 bytes; the atlas becomes a texture array of one page side, the bitmap font in reserved layer 0 and the skin's pages after it (R7), uploaded with their mips as `Rgba8Unorm`, with a linear and a nearest sampler chosen per quad. `AtlasLimits` comes from `Device::limits()`. The uv table and the uploaded pages have one owner, and the memory budget counts the old atlas alive during a reload and the upload staging.
- Premultiplied alpha throughout: every pipeline, the bitmap font and `text_screen` (today `ALPHA_BLENDING`, crates/render/src/renderer.rs:799).
- The canvas projection replaces the letterbox (renderer.rs:678), and `FrameInput` carries the target size.
- `PlaySkin::prepare` gains the play context and the target's pixel size; `DeclSkin: PlaySkin` uploads the atlas pages in `prepare` and delegates `build` to `skin-model`.

### Tests

**macOS and Linux:**
- Expressions: unit tests, range and bound errors, and a property test against a reference interpreter.
- Tracks: the endpoint table of R2.
- The reducer: effect scripts split across drains at every point, long-note feedback, `failed_at`, retry.
- `lanes`: `SimpleSkin`'s long-note, culling and density tests ported with their literal values unchanged; the scroll copy against a reference copy of render's `Scale` (equal bits) with a drift guard on render's source, removed in S14.
- Golden quad lists and preview PNGs at 16:9, 21:9 and 16:10 with a bundled test font, a resize of one prepared play, adjusted regions, an alpha/add/alpha overlap that fails if blend order is lost, and two same-blend elements with different clips followed by an unclipped one, checked by expected pixels.
- `text`: the load checks and the font file checks at their bounds, the requests (the fields' join, cleaning and cutting, the em), the geometry table with overhanging ink, and a property over valid lines (a squeezed line inside its box, a spilled one at its width, each edge on the box).
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
| S5 | Canvas, regions, layout and its clamp, rotation with the 9-point pivot, `Skin::variant`; the `lanes` widget with the scroll copy | [mac] |
| S5b | The `number`, `judge` and `gauge` widgets: glyph strips, the judgment display with its combo, gauge looks and motions; the widget quad cap and capacity | [mac] |
| S5c | The `text` widget: `TextDef` (chart fields, literals), font files, prepare-time requests, the text table in `Play`, its quads; the text element cap | [mac] |
| S6a | `skin-beatoraja`: the lenient JSON reader, resolution of includes and branches, the header; the input caps; skin-model's confined folder reads; the fuzz target | [mac] |
| S6b | `skin-beatoraja`: mapping JSON onto the existing model; enforces the element, `when` and key limits cumulatively while building, before compiling or allocating each item; the conversion report; the import bench | [mac] |
| S6c | `skin-model` growth driven by S6b's report, with the importer mappings: frame by expression, per-digit offsets, the schema fields the report needs (rate, BPM, remaining time, cover, lift and hi-speed values, gauge kind), images that follow the lift and cover, optionally the subtract, multiply and invert blends; in-play code, so reviewed with its allocation tests | [mac] |
| S7 | `skin-beatoraja`: Lua, sandbox, conversion, report tool | [mac] |
| S8 | `skin-cli`: `check`, `preview`, goldens; the text rasterizer and text layer packing; the test font | [mac] |
| S9 | Lane input in `PlayView`; fast and slow counts in `Tally` | coordinated with stage 2: it changes `game` and `eval`, and `PlayView` literals in render |
| S10 | Render integration: the target size in `FrameInput`, the canvas projection, text layer upload; `SimpleSkin` stays the default | after stage 2 |
| S11 | Player: skin choice, layout settings, adjust mode | after stage 2 |
| S12 | `skin-toml`: the authored format; nine-slice and the @2x syntax; the same cumulative limits as S6b | [mac] |
| S13 | `skin-export` and `build` | [mac] |
| S14 | The standard skin; `SimpleSkin` removed with the scroll drift guard | |
| S15 | BGA | |

The JSON import is complete only after S6c: S6a and S6b alone do not meet it. S6a to S11 make existing beatoraja skins playable before S12 starts the authored format. The exit (design.md "Roadmap"): an existing beatoraja skin and the standard skin both play, and playing with the standard skin is fun.

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
