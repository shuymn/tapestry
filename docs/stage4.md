# Stage 4 Design Draft: tapestry's Own Skins

Stage 4 of the roadmap (design.md "Roadmap") gives tapestry its own skins: the declarative custom skin format (design.md "Custom Skin Format") with its compiler, its beatoraja export and the authoring CLI; the player's layout adjustment of a skin in the format; and the standard skin written in the format, for play. Until it lands, the default play look is `SimpleSkin`. Stage 3 (docs/stage3.md) comes first: it makes existing skins usable through the importers and builds the runtime model that the importers and this format's compiler both target (its §2), canvas, regions and the layout clamp included.

**Status.** Draft. The format's design is not started. When it is, it decides each need the importers found in real skins (docs/stage3.md §3 and the conversion report), taking it in or leaving it out with the reason; the format need not express everything they needed, and every construct it has keeps its export rule to a beatoraja skin. The standard skin is meant to get an editor and a format that image generation can feed (design.md "How We Build It"). The sections below are the plan as far as it is written, and change with that design. R1 to R7 name the runtime model's sections in docs/stage3.md §2, and S6b-1 a row of its pull requests. This file is removed when stage 4 exits, once its decisions are in design.md.

**What the format must do:**
- **A small format of its own.** Few people make skins today. A spec that is large, or has features an author gains nothing from, goes unused. Every concept an author learns has to pay off for them.
- **One source, two skins.** A skin written in tapestry's format also builds a beatoraja skin, so one piece of work reaches the players of both. The beatoraja skin is a faithful subset: tapestry's may do more, and what beatoraja cannot express degrades in the export and never breaks it.
- **Not bound to 16:9.** The format adapts to other aspect ratios such as 21:9. The beatoraja export stays 16:9.
- **The player adjusts the composition.** A home setup is not a cabinet. Players move their monitor or where they sit because a skin fixes the whole layout and the existing players can only stretch the screen. Here the player moves and resizes the parts of the screen, and the skin author does nothing for it.

**Direction:**
- **The authored format is specified on its own terms.** Its semantics are written for tapestry; the beatoraja export is a constraint every construct must meet (it has an export rule), never the source of the semantics.

## 1. Scope

**In:**
- **The authored format**, its compiler and diagnostics.
- **The beatoraja exporter** and an authoring CLI that checks, previews, and builds both skins in one command.
- **Player layout adjustment**: per-region position, scale and visibility, lane width, and an adjust mode drawn with the play screen's sprites.
- **The standard skin**, written in the format with self-made art, and the removal of `SimpleSkin`.

**Out:**
- **Non-play screens** in the authored format, its export and the standard skin: these stay play-only at first, every non-play kind keeping its imported skin or its built-in screen.
- **Mouse dragging** in the adjust mode (it comes with `egui`).

## 2. The authored format (`skin-toml`)

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

## 3. The beatoraja exporter (`skin-export`) and the CLI (`skin-cli`)

- **Target:** a generated `.luaskin` at exactly 1920×1080 (a test asserts the size: an unlisted size silently falls back to 1280×720 in both loaders), using only the fields and runtime API that upstream and ED share. Conditions are `draw` Lua strings, because ED's `op` takes integers only. The loaders compile each such string on its own in the skin's global environment, so the prelude helpers are installed as globals with a reserved prefix, not as locals of the generated file. A flag for upstream-only fields can come later. JSON was rejected: it cannot compute geometry and expands long.
- **Regions** become one user offset per region. The Lua reads the configured values at load (ED exports them before the configured pass, `LuaSkinLoader`, `SkinLuaAccessor`) and bakes each region's transform into its elements' keyframes; no destination offset applies them a second time. The built-in offsets are used only where their meaning matches: lift and lane cover for the covers. The whole-skin offset scales the lanes and BGA too, so it does not stand in for a HUD region.
- **Text:** literal text is baked into images at 1x. Chart-bound text exports as `text` plus `font`, copying the author's font file, which must be redistributable and cover the needed glyphs: `size` is the largest key `h`, `Shrink` is `overflow` 1 and `Spill` 0, and the destination x moves right by `w·{0, ½, 1}` for left, centre and right. Chart-bound text in the default font cannot ship a redistributable font, so it is left out with a report line. A title cannot be baked into a reusable skin.
- **Judgments and gauges** map by a table: tapestry's grades, empty POOR and the preset's gauges to beatoraja's grades and gauge types; a gauge with no counterpart maps to the nearest type with a report line. A `gauge.kind` comparison is remapped explicitly onto the target's gauge types, never exported as the enum's storage, and one the target cannot express is reported.
- **Score rates** export by recomputing the guarded integer formula of R3 from the target's own score and note counts, never by rebuilding basis points from its rounded rate properties (102 and 103, 115 and 116); a rate expression the target cannot compute that way is reported.
- **Widgets** export as the target's `value`, `judge` and `gauge`, and what differs is reported:
  - Numbers: a `Ten` image whose frame count is a multiple of 24 is rewritten, since the target tries 24 first: a cycled strip is split, keeping its set boundaries and wrap, and padding is allowed only for one set or no cycle. `Eleven` with blank or zero padding is reported (the target forces glyph padding) or rewritten to `Ten`. The target hides `i32::MIN` and `i32::MAX`, which is reported.
  - Judgment displays: a `TwentyFour` combo is not exportable (the target's judge takes 10 or 11 glyphs) and is reported. A `Bottom` combo exports directly; a `Top` combo exports only where, per effective state (each slot and the full-gauge PERFECT, grade and combo resolved), every grade key has one height, giving `combo y = grade h − cy − combo h` per key; when a fallback-shared `Top` combo needs different offsets in different states, separate combo entries are written (grade 0 of height 80 and grade 6 of height 100 sharing a `Top` combo at y 10, h 20 give offsets 50 and 70). Otherwise it is reported as not representable.
  - Gauges: a part with an animated image or a non-zero angle is reported (or baked to frame 0 and angle 0 with a report line). Each motion exports as the target type of its name, `Still` as random with range 0, and the report states the motion differences (R6).
- **What degrades:** aspect (16:9 only), the side inset, mipmaps and @2x (the 1x image), nine-slice (baked per size, edges rounded so seams do not open).
- **CLI `tapestry-skin`:** `check`; `preview`, which draws the compiled quads with `tiny-skia` into PNGs at 16:9 and 21:9 for a fixture state, so authors preview on macOS and Linux; `export beatoraja`; `build`, which does all three in one command; `watch`, which polls file times as preset files are polled (design.md "Tuning Without Restart"). The preview lives in the CLI, so the player does not link it.
- **In the player** a changed skin is reloaded between plays, built off-thread; a failed reload keeps the current skin, and nothing is polled during play.

## 4. Crates, tests and PRs

### Crates

| Crate | Contents |
| --- | --- |
| `skin-toml` | The authored format, its compiler and diagnostics |
| `skin-export` | The beatoraja writer, from the published format only |
| `skin-cli` | The authoring CLI and its preview |

Like stage 3's skin crates, all are `GPL-3.0-or-later` (design.md "Licenses") and portable: each goes into `REQUIRED_PORTABLE`, the baseline workspace of `xtask/src/fixture.rs`, the license table of `check-arch`, and its in-play functions into `IN_PLAY`. A skin that tapestry exports is the author's work and data, not a derivative of tapestry.

### Tests

**macOS and Linux:**
- Export: both load passes, run under a Lua stub with and without `skin_config`; the generated properties executed against known states, checking values and visibility as well as geometry, with stated pixel and millisecond tolerances; the export size.

**On the developer's PC:** for every change to the format or the exporter, the standard skin's export loads and plays in pinned releases of upstream and ED (0.8.9 and 0.4.0 at the time of writing; the pins are bumped deliberately).

### Pull requests

| PR | Content | Start |
| --- | --- | --- |
| S12 | `skin-toml`: the authored format; nine-slice and the @2x syntax; the same cumulative limits as S6b-1 | first |
| S13 | `skin-export` and `build` | after S12 |
| S8 | `skin-cli`: `check`, `preview`, goldens; the text rasterizer and text layer packing, with `attach_bitmap_text` beside `attach_text` in the preview; the test font | after S12 |
| S16 | Player: layout settings and the adjust mode for a skin in the format (docs/stage3.md R5, "Storage" and "Adjusting"): per-region position, scale and visibility, and lane width, one file per skin under the data root's `layouts\` | after S12 |
| S14 | The standard skin; `SimpleSkin` removed with the scroll drift guard | after S12, S13, S8 and S16 |

## 5. What stage 4 ships

| Wish | Ideal | Stage 4 | Later |
| --- | --- | --- | --- |
| Player layout | Drag anything | Seven fixed regions, lane width, key-driven adjust mode, numbers in the file; imported skins not adjustable | Mouse drag with `egui`; per-region opacity |
| Export | Everything | 16:9 1080p `.luaskin`, shared field set, regions as offsets, a report of what was left out | 16:10 and 4:3 bakes, an upstream flag; aspect, mips: never |
