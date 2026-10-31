# Stage 2 Implementation Plan: The Full Gameplay Loop

Stage 2 of the roadmap (design.md "Roadmap") is one full gameplay loop: song selection, results and retry; practice (section repeat and speed change); beatoraja-style and LR2-style presets, switchable on the result screen; `eval-cli`; and development features such as scenario launch.

The stage exits when song selection, play, results and retry can be played repeatedly and the property tests pass. Tracking issue #70; progress issue #4.

**Citations.** `file:N` points into the repository at commit ea213e2, and design.md is cited by section name. Code comments cite design.md, never this file. This file is removed at the stage exit, once every decision below is in design.md; each PR updates design.md first (AGENTS.md).

**Direction applied throughout:**
- Every extension point keeps one named interface, and core calls through it.
- Nothing is built ahead of the PR that needs it.
- Measured figures are indicators, never gates.
- Verification runs on the development PC.
- A flaw found by a later variation rebuilds the affected part.

**Decisions that change what players see**, each reflected below:
- the LR2-style preset, its long-note rule included, follows LR2oraja Endless Dream (D11);
- "-style" presets reproduce their target's rules, not its machine, and each lists the result differences this causes, such as a clear lamp that fixed-point rounding changes (D11);
- the chart keeps its `#RANK`, `#DEFEXRANK` and `#TOTAL` declarations and each preset reads them, so one chart can have different windows and `#TOTAL` under different presets (D11, PR7e);
- a what-if of a practice record transfers initial gauges by gauge ID, and a gauge the record lacks, or whose recorded value it does not accept, starts at its preset's start value, marked as such (PR23);
- running `tapestry` with no arguments opens song selection (D1);
- the library database is rebuilt when its schema version changes (D5);
- no egui in stage 2, and library roots live in `device.toml` (D4);
- difficulty tables, song preview and pitch-preserving speed are deferred (§1);
- personal bests for charts with `#RANDOM` are left undecided, and such charts get no best in stage 2 (D8);
- a what-if of a completed play runs to its own final deadline (D9);
- the gauge denominator in practice is the whole chart's note count (D13);
- a practice round ends with a short ring-out once the section is judged: the BGM plays on without notes, fades out, and the next round starts (D15);
- the standard presets (tapestry, beatoraja-style and LR2-style) are built in and chosen by name, beatoraja-style by default; a preset the player changes is a user preset file in `presets\`, created for now with `eval-cli preset export` (D24).

## 0. Prerequisites

#84 and #85 land before the first stage-2 PR.

- **#84** moves the reservation before the countdown and gives retry and practice one shared play-start flow. PR14 and PR23 build on that flow.
- **#85** makes input-thread failure a typed error. Today `InputLink` calls `expect` (crates/player/src/input_side.rs:81-118). PR9 and PR10 add requests and a wake event on top of the error path #85 creates.

## 1. Scope

**In:**
- **Song selection.** A library of the configured folders, scanned in the background (only changed files are read). Literal substring search with width and kana folding, sorting, and choosing a chart to play it.
- **Results.** Judgment counts, EX score, BP, max combo, every gauge with its history, the early/late distribution, the difference from the personal best, and the recommended offset. The result can switch to the evaluation under any other preset.
- **Retry and back to selection,** across charts.
- **Standard presets,** built in: beatoraja-style, which becomes the default, LR2-style and tapestry. User presets as files (D24).
- **Preset polling,** with immediate re-evaluation on the result screen.
- **Practice.** Speed change (50-150 %), mid-chart start, section repeat, and section evaluation with its excerpt property.
- **Tools.** `eval-cli`, `--dev-scenario`, and a specified start position.
- **Input cadence.** The #87 wake policy and an idle cadence.
- **Measurement and audio.** `criterion` benchmarks and loudness normalization (#51).

**Out:**
- **Stage 3:** visuals and effects, the declarative skin, BGA.
- **After stage 3:** the LR2 and beatoraja skin adapters, as variations. IR, the distribution registry, anti-cheat and servers stay untouched until stage 3 is done (#4).
- **Decided deferrals:**
  - difficulty tables (importing table data, fetched or from a local file, plus an MD5 join);
  - song preview playback;
  - pitch-preserving speed change (needs a time stretcher, so in stage 2 speed changes pitch);
  - `egui` (D4);
  - how personal bests treat charts with `#RANDOM` (D8);
  - creating and editing presets in a game screen, which arrives with an in-game settings screen (D24).
- **Not built yet:** CN/HCN, automatic offset adjustment, and its recorded stream (section evaluation uses fixed offsets).
- **Unchanged by this stage:** #43 output streaming (D16), #45, #75, #81, #83, #86.

## 2. Decisions

Each decision names the alternatives it beat. "Tuned by playing" marks a starting value the developer changes by playing.

### D1. The scenes are a state machine in `game`, driven through a rig

**Today.**
- `live::run` plays one `Loaded` until quit (crates/game/src/live.rs:177-218).
- `Loaded::load` takes one path (crates/game/src/session.rs:391-465).
- The player builds the bank and the renderer once per process (crates/player/src/main.rs:460-472, 769).

**Decision.** `game::app` owns the flow Select → Load → Plays (countdown, play, tail, result, retry) → Select. It is portable and runs headless on a virtual rig.

- **The rig.** `AppRig: Rig` adds:
  - `choose()`;
  - `load_chart(&Loaded)`, which swaps the bank and the renderer's chart;
  - `idle_wait(deadline)`;
  - `show_refusal(&LoadRefusal)`;
  - `entered_selection()`, called on every entry into selection.
- **Plays of one chart.** `live::run` becomes `run_chart`, which returns `Leave::Back` or `Leave::Quit`.
- **A refused load returns to selection.** Today these errors propagate out of `main` and end the process. The screen shows the reason, the threads keep running, and the chart stays selected. The refusals:
  - a chart that does not load;
  - a bank that does not load, including a refused resampler workspace (D14);
  - a missing preset file (D24);
  - `NoJudgeableNotes`;
  - a mapped chart outside the evaluator's domain;
  - `RecordingTooLarge`.
- **Fatal failures.** Only the screen and audio failures `LiveError` already treats as fatal end the run.
- **Quit and close.**
  - `Command::Quit` means "leave the chart" (crates/game/src/session.rs:224-225).
  - From selection, quit returns to selection; in `tapestry play <chart>` it ends the process.
  - Closing the window ends the run from any scene.
- **Menu commands.** Retry, Back, NextPreset/PrevPreset and Practice form `MenuCommand`, separate from the in-play `Command`.
- **The library stays outside `game`;** the rig hands it paths.
- **Session events.** `SessionEvent` (crates/game/src/policy.rs:61-80) gains `Select` and `Result { attempt }`.
- **Launch (decided).** `tapestry` with no arguments opens selection. Today it exits 2 (crates/player/src/args.rs:93).
- **Alternatives:**
  - per-screen loops in `player`: no headless tests;
  - a generic scene stack: four fixed scenes do not need it;
  - a process per chart: it would lose the warm threads and the device state.

### D2. The threads stay up across charts

- **Lifetime.** The window, input, audio and render threads live for the whole run.
- **New messages.**
  - `AudioCtl::Load(Arc<SoundBank>, schedule)`: today `AudioCtl` has no load (crates/game/src/audio_loop.rs:54-71).
  - `RenderMsg::Load(Arc<Chart>)`: today the chart is fixed at spawn.
- **Loading.** The bank is decoded on the loading pool.
- **Retry** reuses the decoded audio and GPU resources.
- **Alternative: rebuild the threads per chart.** That restarts the device state, the focus latch, MMCSS and the WASAPI stream (100 ms or more).

### D3. Japanese text uses `cosmic-text` and `glyphon`, as design.md says

**Today.** End-screen text is a 5×7 ASCII bitmap font (crates/player/src/render_side.rs:159).

**Decision.** `render` gains a `text` module.
- `cosmic-text` lays text out. `glyphon` 0.12 (wgpu 30, like the workspace) keeps the atlas and draws in its own pass after the sprite pass on menu screens.
- Text is prepared on menus only, where allocation is allowed.
- The play screen keeps the bitmap font.

**Fonts.**
- **Runtime.** The first present of Yu Gothic UI, Meiryo and MS Gothic; only those files are read.
- **Golden image test.** It pins `YuGothM.ttc` by SHA-256. A different hash fails the test and names both hashes. The expected image is updated after review, as for WARP (setup.md "CI").

**Alternative: cosmic-text glyphs in our own sprite atlas.** We would own atlas packing, eviction, glyph keys and subpixel positioning, which glyphon already does. No verified reason argues against glyphon.

### D4. No `egui` in stage 2 (decided)

Settings are TOML files plus cycles on the select screen, drawn with D3:
- **Library roots** live in `device.toml` under `[library] roots`, because they are per PC.
- **The play preset** is cycled on the select screen and written to `preset` in `player.toml` (crates/game/src/settings.rs:628).
- **Practice options** are set on the practice screen.

egui comes with the first form-like screen.

### D5. The library crate and database

**The crate.** `library` is L4, GPL-3.0-or-later and portable, on `rusqlite` (bundled, FTS5). It is added to `LICENSES` (xtask/src/arch.rs:47), `REQUIRED_PORTABLE` (xtask/src/portable.rs:16) and the fixture.

**The database file.** `library.db` lives under the data root, in WAL mode. The scan thread owns one writer connection; the game thread owns one reader connection and queries only outside play.

**Schema.** `PRAGMA user_version` holds the version, bumped by one at each merge that changes the schema:

| Version | PR | Adds |
| --- | --- | --- |
| 1 | PR12 | `roots`; `charts(path UNIQUE, size, mtime_ns, sha256, md5, header fields)` |
| 2 | PR13 | `norm_text` (the searched fields normalized and joined); `title_key` and `artist_key` (normalized sort keys, so full-width and katakana titles sort with the rest), with the index `charts_title` for the unfiltered title order; `folder` (the path's folder, for grouping without string work per row); `charts_fts` |
| 3 | PR18 | `meta`, `scores`, `counted` |
| 4 | PR25 | `loudness` |

**On a different version (decided).** Everything in the database is derived, so the game deletes the file with its `-wal` and `-shm`, rebuilds it by a full scan, and logs one line. Only the version number is read from the old file.

**Alternatives.** Our own index files; `sled` or `redb`, which have no full-text search.

### D6. Background scanning

- **One scan thread (decided).** It lists `.bms`, `.bme` and `.bml` files, and in turn reads every file whose `(path, size, mtime)` changed (16 MiB cap), computes SHA-256 and MD5, calls `bms_parser::read_header`, and commits.
  - `read_header` keeps only top-level header commands; it resolves no `#RANDOM` and computes no time.
  - Why: pausing, shutdown and failure live on one state that only this thread changes, so no acknowledgment or end can be lost between threads.
  - Cost: reading does not overlap hashing and committing (a full scan of 300 generated files: about 136 ms, against 86 ms with separate walker, parser and writer threads).
- **Writing.** Commits in batches of 256. Rows for missing files are removed.
- **Pausing.**
  - Choosing a chart requests a pause before loading.
  - The thread finishes the file in hand, commits, and acknowledges.
  - The countdown starts only after the acknowledgment.
  - Every entry into selection releases the pause (after a refusal, Back, or quit from play), and so does entering a result screen.
- **Priority.** A start hook from `player` sets background mode, so Windows code stays in `player`.

### D7. Search and sort

- **Normalization**, applied when indexing and when querying: C0 control characters dropped first, then NFKC, lowercase, and katakana folded to hiragana (U+30A1-U+30F6 minus 0x60).
  - Why the controls go first: neither side then holds NUL, at which SQLite's `LIKE` and string parsing stop, and a control between a base and its combining mark does not keep them from composing.
  - The searched fields (title, subtitle, artist, subartist, genre) are joined by U+001F, which no normalized query holds, so no match spans two fields.
- **Characters** are Unicode scalar values of the normalized query, the trigram tokenizer's unit. A query that normalizes to nothing matches every chart.
- **Long queries (3 or more characters).** The query becomes one FTS5 string: every `"` is doubled, the whole is wrapped in `"…"`, and it is bound as `charts_fts MATCH ?1`. The candidates must also contain the normalized query exactly (`instr(norm_text, ?2) > 0`), since the tokenizer reads U+FFFE and U+FFFF as U+FFFD; both conditions apply before `LIMIT`, so the limit counts only matches.
- **Short queries.** `norm_text LIKE ?1 ESCAPE '\'`, with `\`, `%` and `_` escaped.
- **Not exposed.** SQLite's query syntax is never exposed, and there is no transliteration.
- **Sorting.** By normalized title, normalized artist, level (digit-only levels by value at any length, then other text, then none), or file modification time (newest first). The path is the last key, so the order is total. The title order is indexed.
- **Grouping and input.** Charts are grouped by folder. Search text arrives through winit IME while the search box has focus.

### D8. Personal bests: key, eligibility and invalidation

**Key.** Bests are keyed by (file SHA-256, preset SHA-256, LN mode).

**Charts with `#RANDOM` get no best yet.** Whether a best is kept per resolved branch or per file is left open: a file whose branches hold 10 and 100 notes would otherwise either show several bests or compare a perfect 10-note play with a 100-note branch's 200 EX.
- A play whose load resolved any `#RANDOM` is saved as usual, but `counts_for_best` is false for it and selection shows no best for the chart.
- Records keep their seed and resolved-chart hash, and the bests are a cache rebuilt from records. Either rule can therefore be adopted later by a schema bump and a rebuild, with no play lost.

**Eligibility.** One predicate, `counts_for_best(record, verified, saved)`, serves live updates and rebuilds alike. It holds only when all of these are true:
- `verify` accepts the record;
- the record is not incomplete;
- the play ended `PlayedToEnd`;
- it was a full play at 1×;
- the record was saved.

**Values.**
- `scores` holds the best EX, the best BP, a lamp, and the play count.
- The lamp is the hardest gauge cleared (easiest first) or FULL COMBO.
- The play count counts every saved, verified record under the key, so plays of a `#RANDOM` chart count across its branches even though they set no best.

**Idempotence.** One ingest function serves live updates and rebuilds. `counted(record_file PRIMARY KEY)` makes ingesting the same record twice a no-op.

**Invalidation.**
- `meta` stores the interpretation tuple: replay format, canonical encoding, evaluator and loader versions.
- If the tuple differs on open, `scores` and `counted` are cleared before anything is shown, and a rebuild runs in the background.
- Refused records are counted by reason.

### D9. The result screen re-evaluates the same input under any preset

**Today.** `ReEvaluate` refuses another preset hash (crates/game/src/results.rs:74-101, :94) and stops at the recorded watermark (:89).

**Decision.** `ReEvaluate::under(ctx, preset)` checks the evaluator version first, then evaluates in one of three ways:
- **As played** (equal preset hash): to the recorded watermark. The result must equal live.
- **What-if of a completed play** (decided): to the what-if preset's own final deadline. Input ends with the recording, and the screen shows "no input after <t>".
  - Example: the last note is at 60.000 s, hit on time. The tapestry preset's 180 ms late BAD edge closes the recording at about 60.180 s. A preset with a 220 ms edge has its final deadline at 60.220 s.
  - That what-if completes and clears, with 60.180-60.220 s marked "no input".
- **What-if of an aborted play:** to the recorded watermark, labeled "ended midway at <t>", with no clear.

**The shelf.** `game::presets::Shelf` holds the built-in presets and the user presets in `presets\*.toml` (D24).
- Other presets are evaluated lazily, cached by hash.
- Switching presets on the result screen only changes the view. The play preset changes on the select screen, for the next play.

### D10. User preset files are polled, not watched

**Today.** The shelf (crates/game/src/presets.rs) checks the user preset files every 250 ms while a result screen shows (`Loaded::poll_presets`) and at every retry; the select screen does not poll yet (PR17).

**Decision.** While a select or result screen shows, the shelf checks the user preset files' modification times every 250 ms. The built-ins are never read from disk, so they are never polled.
- A changed file reloads, and the result screen re-evaluates under it.
- A file that fails to load, or whose name is a built-in's (D24), leaves the previous preset in place and shows the error.
- Plays pick up changes from the next play; nothing is polled during play.

**Alternatives.** `notify`, or `ReadDirectoryChangesW`.

### D11. The standard presets and their rules

**Where they live.** `rules::beatoraja_style()` and `rules::lr2_style()`.

**Targets (decided).** beatoraja-style follows beatoraja (exch-bms2/beatoraja@83574048) and LR2-style follows LR2oraja Endless Dream (seraxis/lr2oraja-endlessdream@d4995153), whose LR2 rule set is a set of changes on beatoraja's judge code. Lunatic Vibes and bmz-player are references only: evidence for which axes exist, not targets, so behavior only they have is not built.

**The contract.** design.md "Rule Presets" holds it: preparation, one shared scheduler, press resolution, LN transitions and accounting ("Evaluation Stages"); closed policies with their own data, combined only where specified and tested, with a dedicated implementation for a substantially different state machine; one canonical encoding per validated preset value ("Policies, Combinations and Versions"). Every PR below fits its policy into the stage that owns it.

**Values and examples.**
- Values come from the target's source, each cited by file and line in the preset's module and its test file.
- Each preset gets a few dozen hand-written examples.
- A policy variant arrives only with a hand-written example from its target preset that fails without it.

**Rules, not the machine (decided).** Evaluation stays integer µs with fixed-point gauges and event-time processing. Each preset's module lists, beside its gaps, the known differences this causes, each with an example (design.md "What a -style Preset Reproduces"):
- `f32` gauge rounding: from 20 %, 600 increases of 0.1 % reach about 79.9993 % in `f32`, below an 80 % clear, and exactly 80 % here;
- the latest change per key per poll, and a poll's key changes before its misses;
- tapestry's own scratch conversion.

**Implementation structure is not reproduced (decided).** A behavior that comes from a target's implementation structure, and would not arise from a straightforward implementation here, is not built; the preset's module records it as a known difference with its mechanism and an example. beatoraja-style's:
- **The note scan's far end:** beatoraja scans a lane's notes up to the farthest early edge of its note and scratch tables, MS pairs included, and stops at a note exactly that far (`play/JudgeManager.java:189-196`, `:387-389`). Here a press looks at its lane class's own BAD window and empty POOR range, each with its own edges. So when a press judges no note, a note exactly 500 ms ahead makes an empty POOR here at every scale, and there only once a scaled early BAD edge passes 500 ms; and at `#DEFEXRANK 292` a scratch press exactly 503.7 ms early is BAD here and judges nothing there.
- **`#DEFEXRANK`:** read as a number above 0 (`120`, `120.0` and `120.5` alike), counting over any `#RANK` before or after it (jbms-parser a7a0729 ignored `#RANK` once a `#DEFEXRANK` was set, and beatoraja's chart documentation says it takes precedence; the check went away in d0fba6d, when the `>= 10` encoding stopped working), and its `n × 75 / 100` % applied to each edge as it is, not truncated to an integer percent (`play/BMSPlayerRule.java:59-65`, under a TODO): `#DEFEXRANK 1` gives PERFECT ±150 µs here, zero-width windows there.
- **H. Miss timing:** a note is missed at its late BAD edge, after the inputs at that µs; beatoraja's first frame strictly past it is its frame loop's, so a press on another lane at `edge + 1 µs` comes after the miss here and before it there.
- **Notes a press passes (decided, B's second half):** when a press takes a later note, the earlier unjudged notes of its lane are POOR at the press, as tapestry does. beatoraja keeps them hittable until their own late BAD edge, a consequence of its structure with no stated intent: its judge algorithms (caef58b5, then d9b70361 and d19e05b5) only choose which note a press takes, and misses are a separate timeout pass. At `#RANK 3`, with notes at 0 and 200 ms on one lane, a press at 170 ms takes the 200 ms note; a second press at 250 ms is BAD on the 0 ms note there, while here that note was POOR at 170 ms.
- **G. A BAD long-note head (decided):** it is scored at the press, when its judgment is decided, since a BAD head cannot get worse; beatoraja holds it and scores it at the release or the end, so max combo can differ.

**What beatoraja-style still needs.** Nothing the model cannot express: PR7 landed the values the model expresses, PR7b combo priority (B: which note a press takes, `Assignment::ComboPriority`; the notes it passes are a known difference above), and PR7c the gauges (C, D, E: HARD guts as ordered damage bands; the HARD and EX HARD increase scaled by `max(0, min(0.15, (2T − 320) / N)) / 0.15`; `#TOTAL` when absent as `max(260, 7.605N / (0.01N + 6.5))`, with a `#TOTAL` not positive ignored so an earlier one stays, jbms-parser `BMSDecoder.java:730-743`, from PR7e; N the whole chart's count, practice included). Each is shown by an example in crates/eval/tests/style_beatoraja.rs, cited under `src/bms/player/beatoraja/`.

**What LR2-style adds.** `rules::lr2_style()` (PR8) assembles combo priority (B), the variants below and Endless Dream's values, each shown by an example in crates/eval/tests/style_lr2.rs. Endless Dream shares B, the notes a press passes, G, H and the `#DEFEXRANK` reading as known differences, and its single low-gauge step (× 0.6 below 32 %) fits the band list. Its own known differences, cited at that commit under `core/src/bms/player/beatoraja/`:
- **One press judges one note (decided).** Endless Dream's multi-BAD judges BAD, with the same press, the other unjudged notes in BAD but not GOOD range, filtered by the primary's timing, judgment and LN type, leading LN heads trimmed (`play/JudgeManager.java:993-1061`, from wcko87/lr2oraja bbe35f5e). That approximates LR2, where the lane pointer advances past a BAD and the same press judges the next note again (OpenLR2 `ProcSinglenote`, GOMazk/OpenLR2@583a3850 `LR2f.cpp:7509-7523`), an implementation artifact. Here the notes a press passes are POOR at the press. At `#RANK 3` (100 %), with taps at 0 and 160 ms and a long note from 50 to 100 ms on one lane, a press at 190 ms takes the 160 ms tap (GREAT); Endless Dream gives BAD on the 0 ms tap and the head, LR2-style POOR on both at the press.
- **An ineligible LN head keeps the earlier candidate (decided).** Endless Dream makes a late BAD head ineligible (K) by giving it judge 6 (`play/JudgeManager.java:435-437`), which reuses beatoraja's "no judgment here" branch, and that branch also resets the candidate the scan had already taken (`:445-452`), an implementation artifact: the rule only says a late BAD head cannot be taken, and LR2 judges the oldest note. Here the earlier candidate stays the primary. At `#RANK 3` (100 %), with a tap at 0 ms and a long note from 30 ms on one lane, a press at 190 ms is BAD on the tap here and the head is missed at 230 ms; Endless Dream judges nothing at the press, and misses the tap at 200 ms and the head at 230 ms.
- **The empty POOR range's far edge.** Endless Dream's MS pair reaches 1000 ms ahead inclusively, but its note scan stops at a note that far (`play/JudgeManager.java:416`, the farthest early edge of the note and scratch tables, `:202-210`), a scan bound, not a window. Here the range is a window in this model, both edges inclusive, with no code to make the far edge exclusive. When a press judges no note, a note exactly 1000 ms after it makes an empty POOR here and nothing there; 1 µs closer, both.
- **`#DEFEXRANK`'s percentage is not truncated.** Endless Dream stores `n × 75 / 100` as an `int` before interpolating (`play/BMSPlayerRule.java:65`); here the percentage is interpolated as it is, as beatoraja-style scales by it. `#DEFEXRANK 1` gives PERFECT, GREAT and GOOD 240, 720 and 1200 µs here (0.75 %), zero-width there (0 %); `#DEFEXRANK 133` interpolates at 99.75 % here (PERFECT ±20.97 ms) and 99 % there (±20.88 ms).
- **A release long after the end.** Endless Dream grades a release on both sides of the end (`play/JudgeManager.java:539-541`) and completes a long note held through its end only at its first frame past it (`:619-620`), so with no frame between the end and a release 121 ms or more after it, that release is BAD there (`:582-590`). Here the long note is already complete at its end with its head's judgment (design.md "LN Judgment Modes"), and nothing is built for it.
- **A long note's timing.** Endless Dream gives a long note finished by a release the deviation of larger magnitude of its head and its release (`play/JudgeManager.java:583-585`) and counts it early or late by it (`:708`). Here the `Judged` event keeps both, the head's deviation and the release's distance from the end, and timing readers take the head's; the event contract is unchanged. At `#RANK 3`, a head pressed 30 ms late and released 100 ms before the end is GREAT in both; Endless Dream records an early GREAT 100 ms off, here a late one 30 ms off with the release at −100 ms (`ln_timing_is_the_heads`).

Its own rules, each with an example in crates/eval/tests/style_lr2.rs:
- **I. Window interpolation** (`WindowPolicy::Interpolated`): below 100 % PERFECT, GREAT and GOOD come from the LR2 table, interpolated; at 100 % and above they scale linearly; then each of them is clamped on each side to the next wider window, so at `#DEFEXRANK 400` (300 %) GOOD is BAD's 200 ms, not 360 ms; the LN-end edges follow the same rule, GOOD clamped to the class's early BAD edge (Endless Dream's LN-end BAD row is the note BAD's 200 ms, `play/JudgeProperty.java:56`, `:58`); BAD and empty POOR are not scaled; `#RANK` 0-4 is 25, 50, 75, 100, 75 %, no rank 75 %, and `#DEFEXRANK n` is n × 75 / 100 % as it is (`play/JudgeProperty.java:169`, `:185-265`, the clamp `:241-249`; `play/BMSPlayerRule.java:61-65`). At `#RANK` 0-3 PERFECT, GREAT and GOOD are 8/24/40, 15/30/60, 18/40/100 and 21/60/120 ms; `#DEFEXRANK 50` (37.5 %) 11.5/27/50 ms; `#DEFEXRANK 80` (60 %) 16.2/34/76 ms.
- **K. LN-head eligibility** (`HeadEligibility::NotLateBad`, PR7d): a late BAD on an LN head cannot be the primary (`play/JudgeManager.java:435-437`); a head the primary passes is POOR at the press, and one it does not pass stays to its own miss. At `#RANK 3` (100 %), with a long note from 0 to 50 ms and a tap at 300 ms on one lane, a press at 160 ms cannot take the head (160 ms late, BAD), which combo priority alone would keep since the tap lies outside GOOD; it takes the tap (140 ms early, BAD), and the head is POOR at the press. With the long note alone, the press takes nothing and the head is missed at its own deadline.
- **L. Empty POOR:** a note 0 to 1000 ms after the press, judged or not, both edges inclusive; no late side; not scaled (`play/JudgeProperty.java:54-55`, `play/JudgeManager.java:414-442`). It is the one range per lane class the model already has: Endless Dream's range for an unjudged note starts past the inclusive early BAD edge, and an unjudged note inside early BAD is always a candidate here, so no press without a primary has one. Its far edge is the known difference above.
- **M. Gauges** (PR7c's variants): HARD-family damage × max(fix1, fix2); a survival gauge strictly below 2 % after a change, mines included, drops to 0 and fails; `#TOTAL` when absent as `160 + (N + clamp(N − 400, 0, 200)) × 0.16` (`play/GrooveGauge.java:218-225`, `:279-309`; `play/BMSPlayerRule.java:88-89`).

**Policies are choices, not replacements (decided).** Each is a new variant beside today's behavior, which stays available: tapestry keeps nearest assignment, misses at the press for overtaken notes, a miss at the edge and a BAD head scored at the press, and reads judgment headers as loader version 6 does, so its results do not change. Only the selection variants a supported preset uses are added (combo priority); beatoraja's other algorithms wait for a preset that uses one.

**Versions.**
- A new field or variant bumps the canonical encoding version (`FORMAT_VERSION` in crates/rules/src/canonical.rs), so older records are refused. A new field also changes the file shape, and older files are refused with the existing hint; a new variant alone adds no key, so a user preset file naming an existing variant still reads with its own values and hash.
- The evaluator version (`Implementation::STANDARD_VERSION`) bumps only when an existing variant's meaning changes.
- The judgment-header model bumps `LOADER_VERSION`; records of loader version 6 are refused, also under an override (design.md "Determinism"), and cannot gain the declarations they did not keep.

**Long notes.**
- **beatoraja-style** uses `LnMode::Ln`: the worse of head and release.
- **LR2-style** follows Endless Dream (decided).
  - A release within the GOOD width before the end keeps the head's judgment; an earlier release is BAD.
  - The data is LN-end edges for PERFECT, GREAT and GOOD all set to that width (120 ms at 100 %, keys and scratch alike), not a separate release policy. Validation is non-decreasing (crates/rules/src/validate.rs).
  - A head at or after the end keeps `CompletesAtPress`. Endless Dream has no rule of its own for it: its end check completes the long note with the head's judgment at the first frame past the end (`play/JudgeManager.java:619-620`), as recorded in the preset's module.
  - A release after the end changes nothing here; Endless Dream's late side is the known difference above.
- The display is unchanged.

**Generators.** The props generators draw every supported policy combination, not a sample, and deliberate examples cover the joint interactions (selection × overtaken notes × empty POOR × LN eligibility).

### D12. Re-evaluating saved records: what moves down, and the refusal order

**What moves down.**
- The preset file loader moves from GPL `game` (crates/game/src/preset.rs:42) to a new crate, `rules-toml` (L2, MIT OR Apache-2.0). `rules` stays free of dependencies (R1).
- `RulePreset::from_canonical_bytes` is a bounded decoder. It refuses another encoding version, counts that exceed the remaining bytes, unknown tags, trailing bytes, and a preset that fails `validate()`.
- A `preset_canonical` fuzz target is added to `fuzz/` and to the `targets` list in scripts/fuzz.sh:19.

**`replay::verify`.** Its entry points are `load_recorded_chart(record, bytes, dir)` and `evaluate_record(record, chart, preset_override)`. It depends on `bms-parser`. The first failure is the refusal:
1. **Decoding** (crates/replay/src/lib.rs): format version (:425), structure, and preset hash (`PresetHashMismatch`, :566). These checks are structural and come first.
2. **Evaluator version.**
3. **The recorded preset copy,** decoded on every path, including with an override.
4. **Loader version** (crates/bms-parser/src/lib.rs:135).
5. **The load,** with the recorded seed and speed.
6. **Resolved-chart hash.**
7. **Known transform:** only `passthrough` v1, whose constant moves from crates/game/src/prepare.rs:115-128.

The file SHA-256 is not compared.

**Callers.**
- `eval-cli` refuses incomplete records.
- The result screen re-evaluates the chart just played, without `verify`.

### D13. Section evaluation

**Shape.** `EvalInput` and `Evaluator::new` take `Option<Section { start, end, lead_in, initial_gauges }>`, a half-open range `[start, end)` in chart time. With `None`, nothing changes (crates/eval/src/batch.rs:13-26).

**Targets.** A tap or mine counts when `start ≤ t < end`. A long note counts when `start ≤ head` and `ln_end < end`. For `[10 s, 20 s)`:

| Object | Counts |
| --- | --- |
| tap at 10 s | yes |
| tap at 20 s | no |
| LN 15 s → 20 s | no |
| LN 15 s → 19.999999 s | yes |
| LN 9.9 s → 15 s | no |

**Input.**
- Events before `start − lead_in` feed input conversion only.
- A lead-in press is judged only inside the first target's early window. Any other lead-in press is ignored, with no empty POOR.
- Finalization runs to the section's final deadline: the latest of the targets' miss deadlines, LN-end deadlines and mines.

**Starting state.** Judgment and gauge state start fresh. Input conversion state comes from the snapshot plus every earlier event.

**Gauge denominator (decided).** The whole chart's judgeable-note count. Example: `#TOTAL` 160 over 1,000 notes; 100 perfect notes from 20 % end at 36 %.

**Excerpt equivalence.** An excerpt is equivalent to a mid-chart start when these are equal at the lead-in start:
- input conversion state (held controls, accumulated scratch movement and hysteresis, pending scratch releases);
- the lead-in input;
- the time mapping;
- the offsets.

### D14. Speed inside the loader's exact time; sound converted in two bounded stages

**The ladder.** Speeds are `k/20` for k = 10 to 30.

**The loader.**
- `LoadOptions::speed` computes each real time as `floor(exact × 20/k)`, a single floor.
- The scroll map is built from the mapped exact timeline.
- The same pass produces the 1× times and the 1× scroll map, and the resolved hash covers the 1× chart. design.md "Chart Model", the hash contract and "Practice Speed Changes" change together.

**Real time.**
- Hi-speed comes from the mapped start BPM.
- Judgment windows stay in real time.
- The duration cap applies at 1×.
- A mapped chart outside ±2^32 µs (more than about 35.8 min at 50 %) is refused through D1.

**Sound.** The bank rate stays the device rate (crates/audio/src/engine.rs:154). Conversion runs in two `rubato` `Fft` stages with integer rates (crates/audio/src/bank.rs:720):
1. **Source → device,** as today.
2. **Speed,** at the reduced rates `k/g : 20/g`, where `g = gcd(k, 20)`, input to output: 1:2 at 50 %, 13:20 at 65 %, 29:20 at 145 %.

A single exact stage is not used. A 383,999 Hz stereo source at 145 % on 384,000 Hz output gives the coprime pair 11,135,971 : 7,680,000, whose buffers alone exceed 512 MiB.

**Workspace preflight.**
- Before a stage is built, the bank computes a conservative upper bound for its workspace from the reduced rates and the channel count. The bound is derived from the pinned `rubato` version's construction: its retained sample, spectrum, overlap and per-channel scratch buffers, its FFT plans and FFT scratch, and the peak of its constructor's temporaries. A `rubato` update re-derives it.
- That bound is charged to the bank cap through the shared counter that also sums concurrent workers' decode charges.
- A sound whose workspace does not fit is refused before anything is allocated.

**Length.** The output length is `floor(ceil(frames × device / r) × 20/k)`: the source → device stage rounds up, as at 1×, and the speed stage rounds down. Example: a 60 s, 44.1 kHz sound played at 48 kHz.

| Speed | Frames | Duration |
| --- | --- | --- |
| 50 % | 5,760,000 | 120.000 s |
| 65 % | 4,430,769 | 92.308 s |
| 145 % | 1,986,206 | 41.379 s |

A fractional first stage: 1,000 frames at 44.1 kHz are 1,088.435 frames at 48 kHz, so 1,089, and `floor(1,089 × 20/13)` = 1,675 frames at 65 %. One 44.1 kHz frame becomes 2 frames at 48 kHz and 4 at 50 %.

**Playback and records.**
- The mixer is unchanged, and pitch follows speed.
- A speed change reloads the bank.
- The speed is recorded, and `verify` loads with it.

**Alternatives:**
- mapping input times: every real-time duration would need scaling inside the evaluator;
- re-flooring an already floored chart: rounding drift;
- a virtual bank rate: drift;
- variable-rate voices in the mixer: in-play code;
- a single exact stage: unbounded workspace.

### D15. Mid-chart start and section repeat

**Rounds.** Each round runs reset → restore → lead-in → section evaluation through #84's flow. Each round is one attempt and one saved practice record.

**Restore.**
- **Schedule.** The BGM plus autoplay key sounds for every note that is not a target: before the section, after its end, and long notes crossing either edge.
- **Audio.** The seek is split by thread.
  - **Planning, before the countdown, off the audio thread.** `SeekPlan::new` takes the engine's `SeekSource` (its id, bank, voice count and a hash of its schedule; taken before the engine moves to the audio thread), the schedule, the play's `FrameLine` and `t`, and refuses a schedule whose hash differs. It decides the schedule's starts, stops, steals and positions before `t`'s frame without summing samples, admitting at most `MAX_PENDING_STARTS` starts per engine pass as playing does. It costs O(starts before `t` × voices): about 2 ms for a dense 2-minute chart and 0.2 s for 500,000 starts with 256 voices.
  - **The plan.** It holds the sounding voices and releases: two 32-byte slots per voice, 16 KiB at 256 voices, whatever the chart. It travels with the play command.
  - **On the audio thread.** `AudioEngine::seek(&plan)` copies the slots into slots preallocated with the mixer, bounded by the voice count, and allocates nothing. It refuses, with nothing changed, a plan from another engine, one on a line of another rate or period, and one whose frame is already written; the caller plans again.
  - **Placement.** At the pass that reaches `t`'s frame on its own mix clock, the voices enter partway into their sounds. The plan's own admissions there count toward that pass's limit.
  - **A moved clock.** If the clock moved since planning, the voices move with `t`'s frame. A start the plan left out waits until the voices enter and never starts before that frame.
  - **What is exact.** The output equals mixing from the start, sample for sample, when from planning to `t`'s frame the clock maps chart times as the plan's line does, every pass writes the line's period, and no key sound is started; a property test checks it under a steady clock. Otherwise the result is degraded but bounded. A moved clock moves the voices with `t`'s frame, and starts land where it puts them, never before that frame, heads intact. A different pass size (a recovery write) or key sounds taking pending starts change only which pass admits the starts beyond `MAX_PENDING_STARTS` in one pass, and the voices those start, cut or steal; each still starts at a pass's first frame, head intact. These are not modelled.
- **Notes.** Only the section's targets are drawn. Notes before the section and after its end, and long notes that cross either edge (which D13 excludes), are not drawn, so the first note on screen is the first one to press. Those notes still sound, as autoplay key sounds on their own times (above), before the section and after it alike.
- **Long notes across an edge** get no special rule: they are neither judged nor drawn. A player who wants one judged moves the section's edge to include it.

**End of a round: a short ring-out (decided).**
- Completion happens in the first step whose watermark covers the section's final deadline. Recording closes there, and the record is frozen; it is saved after the ring-out, as a full play is saved after its tail, so a slow write never delays the ring-out's controls.
- A ring-out follows instead of the full-play audio tail, which waits for every voice and BGM event (crates/game/src/live.rs:359). For `RING_OUT` (1.5 s, tuned by playing) the BGM keeps playing on schedule, no notes are drawn and nothing is judged or recorded; key sounds of notes after the section play as autoplay, like those before it. Over its last `FADE` (0.5 s, tuned by playing) the output fades to silence.
- **The envelope is on output frames.** At completion the game sends `AudioCtl::RingOut { fade_at, end_at }` in play time. The engine converts both to frame positions on the play's mix clock, as it does for sound starts, and applies the gain per frame: 1 before `fade_at`, falling linearly to 0 at `end_at`. At `end_at` it stops every voice and starts nothing more, and it reports the ring-out done like a sealed tail. The fade therefore does not depend on when the request is taken or how the output is divided into periods, and the stop always comes at the envelope's zero. Nothing is allocated in the audio callback.
- **The envelope is reset** to gain 1 by every `Play`, whether the previous ring-out finished or was cut, independently of master volume and loudness gain.
- The ring-out ends when both the engine has reported done and play time has reached `end_at`: the engine mixes ahead into queued output, so its report alone can come early. The record is then saved, and the next round's countdown begins at the following step.
- Quit, retry or focus loss during the ring-out cuts it at once with `AudioCtl::Cancel`, like the full-play tail; the frozen record is still saved. Quit returns to the practice screen.

**Example.** 100 % speed, lead-in 2 s, offset 0, aligned 1 ms steps, 48 kHz output. Section `[10 s, 20 s)` of a 180 s chart; the last target is a tap at 19.5 s; the late BAD edge is 180 ms.
- `SongEnd.time` is 19.680000 s.
- The step at 19.680 s publishes `fresh − 1 µs` = 19.679999 s, which does not cover the deadline.
- The step at 19.681 s publishes 19.680999 s: completion and the close happen there, and `RingOut { fade_at: 20.681 s, end_at: 21.181 s }` is sent.
- The rendered fade covers output frames from 20.681 s to 21.181 s on the play's mix clock: 24,000 frames, whatever the audio period.
- The engine may report done before 21.181 s, since it mixes ahead (with 10 ms periods and two queued, the wake at 21.160 s can render past 21.181 s). The ring-out ends at the first step at or after 21.181 s with the report in, so with aligned steps the record is saved in the step at 21.181 s and round 2's countdown begins at 21.182 s (round 1's timeline), at its own chart time 8.000 s.
- A BGM event at 20.5 s starts during the ring-out; one at 30 s never starts.
- A 60 s BGM voice that started at 5 s is playing 3 s in at round 2's countdown, restored by `seek`, at full gain.

**Setup.** The practice screen picks the section by measure, the speed, the lead-in (2 s, tuned by playing), the initial gauge, and repeat. `--dev-start <ms>` and a scenario step exist in development builds.

### D16. #43 in practice: windows sized by the section's final deadline

**The window.**
- Mapped times are already real time, so nothing is divided by speed again.
- Admission works as for a full play: the plan's probe `Evaluator` is built with the section, and its `final_deadline()` goes into `window_inputs`, with the existing input-offset allowance (crates/game/src/session.rs:601-610).
- The window runs from `s_m − lead_in`, where `s_m = floor(start × 20/k)`, to that deadline.
- Capacity counts whole seconds of window, rounded up.

**Tap example.** Section `[60 s, 70 s)`, last target a tap at 69.5 s, late BAD 180 ms, lead-in 2 s:

| Speed | From | To | Window |
| --- | --- | --- | --- |
| 50 % | 118.000 s | 139.180 s | 21.180 s |
| 65 % | 90.307692 s | 107.103076 s | 16.795384 s |
| 100 % | 58.000 s | 69.680 s | 11.680 s |
| 150 % | 38.000 s | 46.513333 s | 8.513333 s |

**Long-note example.** Section `[10 s, 20 s)`, 100 %, lead-in 2 s, one LN from 18.000 s to 19.900 s.
- The final deadline is the LN end, 19.900 s; the head's miss deadline (18.180 s) is earlier.
- The window is 11.900 s, so capacity counts 12 seconds.
- A tap-only formula would give 10.180 s and 11 seconds.

**Refusal.** A slow full play over 512 MiB is refused before its countdown through D1, with the inputs needed, the inputs that fit, and "practice a section".

**What waits.** Output streaming stays after stage 3.

### D17. The input thread wakes on input; idle means no timer (#87 with #70)

**Today.** Both input threads wake every 1 ms, forever (crates/input/src/win.rs:1188-1227, timer :202-215). The game thread parks at most 2 ms (crates/player/src/play.rs:37).

**Waiting.**
- The thread waits with `MsgWaitForMultipleObjectsEx` on the timer plus a wake event, with `QS_RAWINPUT | MWMO_INPUTAVAILABLE`. `WAIT_FAILED` is a failure.
- The drain, the batch protocol (stamp `S`, push, read `F`, publish `F − 1`) and the same-µs rule (crates/input/tests/sources.rs:150) are kept.

**States.**

| State | When | Heartbeat |
| --- | --- | --- |
| Open | recording | deadline ≤ 1 ms after the last publication, moved only once passed |
| Tail | closed, until the audio tail ends | same as Open |
| Idle | after `Request::Idle` | no timer; wakes for input, device change, request, focus change, stop |

**The wake event is signaled:**
- after every request;
- by the window thread on focus changes, after the latch is set;
- by `Worker::shutdown` and `Drop`, after the stop flag and before the join.

**Bounds.** Input wakes are at least 125 µs apart. A drain is cut at 1,024 messages; the thread publishes, then takes a new stamp.

**Autoplay.** The autoplay/clocked thread is timer-driven while Open or Tail, and event-driven while Idle.

**Reporting and code.**
- `WakeReport` records wakes by cause, heartbeat lateness, and publication intervals.
- `input::cadence` is a pure, portable state machine.
- Records gain the wake policy (format bump, crates/replay/src/lib.rs:65).

**The game thread outside play** parks until it is unparked or until the next UI deadline (250 ms). It keeps the 2 ms park during play and the tail.

### D18. Menu input

- **Keyboard.** Navigation and IME come through winit.
- **Controller.** While Idle and focused, the input thread turns the configured controller's bound lanes and scratch into menu events for the select screen (design.md "Input", "Menu input"):
  - the white keys 1, 3, 5 and 7 confirm (as Enter), and the black keys 2, 4 and 6 go back (as Escape);
  - the scratch scrolls a line at a time: an axis a line for each whole press threshold of movement, taken the short way round the revolution play converts it with, toward higher values down and toward lower values up, the remainder counting toward the next line; a scratch of two buttons, up with the first bound and down with the second;
  - a button sends on its press only, with no repeat while held; only the first connected unit of the configured model sends, and none without a configured controller.
  - Events go through a bounded SPSC queue, and the thread unparks the game thread.
  - Menu events are never recorded.
  - The result screen keeps the keyboard's retry and quit keys; controller commands there arrive with PR17.
- **Keyboard lane keys** are not menu events.

### D19. Record format changes

Each bumps `FORMAT_VERSION` at merge, and older formats are refused:
- the wake policy (PR9);
- the speed (PR20);
- the play type, section, attempt, lead-in and initial gauges (PR23, format 7: the section as evaluated, in real time; the attempt is the play's number in its session).

Each change is covered by round trips and changes D8's interpretation tuple.

### D20. Scenario launch

**The flag.** `--dev-scenario <file.toml>`, built with `dev-tools` only.
- It follows the `--dev-` prefix convention of `play`'s development options (crates/player/src/args.rs). The existing exception is `measure play --perfect`.
- design.md "Shortening Launch-Based Checks" changes from `--scenario`.

**The file.**
- It carries a `version`; any other version is refused.
- Library roots are relative to the file.
- Steps:
  - `select { search | path }`;
  - `play { input = "auto" | log, until = "end" | { at_ms, command } }`;
  - `result = "next-preset" | "prev-preset" | "retry" | "back"`;
  - `capture { at, file }`;
  - `practice { … }` (PR24);
  - `quit`.

**Running.** It runs headless on the real threads. Exit code 0 means every step ran; 1 names the failed step.

### D21. Benchmarks

**What is benchmarked.** `criterion`, with default features off and `cargo_bench_support` on:
- `eval`: batch evaluation of 2,000 and 10,000 notes, and a live advance in 1 ms steps;
- `bms-parser`: loading;
- `render`: `SimpleSkin::build` on a dense frame (Windows);
- `library`: header parsing, and search over 50,000 rows.

**CI.** `cargo bench -- --test` runs each benchmark once. A non-required `bench.yml` compares against the merge base and never fails.

### D22. Loudness normalization (#51)

**Analysis.**
- After load, a background job mixes the 1× autoplay schedule offline through `Mixer::mix_raw`, at unity gain, before volume, headroom and the clipper. The job pauses during play.
- Blocks of 4,096 frames feed `ebur128` in mode `I | TRUE_PEAK | HISTOGRAM`. Histogram mode keeps whole-chart integrated loudness (to bin resolution) with fixed memory. History is not truncated.

**Memory contract.**
- Fixed from the job's start, whatever the chart's duration: the mix block, the analyzer state (histogram, true-peak interpolator, gating), and the mixer's voices.
- For comparison, `RecordingOutput` would need about 1.38 GB for 60 minutes of 48 kHz stereo.

**Cache key.**
- The resolved hash.
- A sound-content digest: SHA-256 over each sound's (id, file SHA-256).
- The mixer and analysis versions.
- The device rate.

Analysis runs at 1× and its result is reused at every speed.

**Gain.** It replaces the 0.5 headroom from the next play: `g = min(10^((−18 − L)/20), 10^(6/20), 10^(−1/20) / P)`, tuned by playing.

| L (LUFS) | P | Terms | g |
| --- | --- | --- | --- |
| −12 | 1.6 | 0.501, 1.995, 0.557 | 0.501 |
| −30 | 0.3 | 3.981, 1.995, 2.971 | 1.995 |

**Unchanged.** The clipper stays. A chart's first play uses 0.5. The gain is not recorded.

### D23. Crate and dependency changes

| Crate | Layer, license | Portable | PR |
| --- | --- | --- | --- |
| `rules-toml` | L2, MIT OR Apache-2.0 | yes | PR1 |
| `eval-cli` | app, MIT OR Apache-2.0 | yes | PR6 |
| `library` | L4, GPL-3.0-or-later | yes | PR12 |

- **Third-party crates:** `criterion`, `rusqlite` (bundled), `cosmic-text`, `glyphon`, `ebur128`.
- **`proptest`** becomes a dev-dependency of `game`, `audio`, `rules` and `bms-parser`.
- **Licenses.** Any license not yet in deny.toml `allow` is added together with its crate.
- **Fuzzing.** The fuzz crate and scripts/fuzz.sh gain `preset_canonical`.

### D24. The standard presets are built in; the default is beatoraja-style (decided)

**Decision.**
- **Built-ins.** The standard presets are built into the code, not files: tapestry (`rules::tapestry()`), beatoraja-style (`rules::beatoraja_style()`) and LR2-style (`rules::lr2_style()`). Each is chosen by its name, `tapestry`, `beatoraja-style` or `lr2-style`, which is also its preset ID, and the game never writes one to disk. The list of built-ins by name lives in `rules`, so `game` and `eval-cli` (which cannot depend on `game`) share it.
- **User presets.** A preset the player changes is the player's own preset, a TOML file in `presets\`. Only these files are read, validated and polled (D10).
- **A user preset's name is its file stem.** The file has no `id` field, and the loader gives the preset its stem as its preset ID.
  - The stem is what `player.toml` and the shelf look a preset up by, and the file system keeps it unique in `presets\`, so it is the name's one source. A name field would be a second name that could disagree with the file's, and two files could claim one name.
  - Renaming the file renames the preset; records then see another preset, since the ID is part of the hash.
  - A file with an `id` field, as every file from an older version has, is refused as unreadable, not converted.
  - A name is 1 to 64 characters of lowercase ASCII letters, digits, `-` and `_`, and a stem that is not one is refused, naming the file: names then compare exactly on every file system, `TAPESTRY.toml` cannot stand for a built-in, and no older value such as `custom.toml` can be a name. The game, the shelf, the exporter and `eval-cli` share this check in `rules-toml`.
- **No passing for a built-in.** A user preset whose stem is a built-in's name is refused, with an error naming the file and the built-in, so no file can pass for a built-in in records. The check is in `rules-toml`'s loader, so `eval-cli --preset` refuses such a file too; `--preset` also takes a built-in's name.
- **`player.toml`'s `preset`** names a built-in or a user preset; its default is `beatoraja-style`. `PlayerSettings` stays `#[serde(default)]`, so a file without `preset` takes the default. Any other value, an older version's file name such as `custom.toml` or `beatoraja-style.toml` among them, is refused with an error naming it and the built-ins; it is not converted (AGENTS.md "No compatibility code before the first release"), and nothing is written.
- **Creating a user preset.** `eval-cli preset export <built-in> <path>` (for example `eval-cli preset export beatoraja-style my-rules.toml`) writes that built-in to a new TOML file, which the player puts in `presets\`. `eval-cli` is portable and does not locate the data root, so it takes a path. It refuses a path whose file exists and one whose stem is a built-in's name, and writes nothing then. Creating and editing presets in a game screen is not built yet: it arrives later with an in-game settings screen (design.md "Libraries"; D4).
- **Nothing writes a preset file but the export.** Nothing writes a preset file at first start or at any other time, except the export command.
- **Where it lands.** PR7 made beatoraja-style the default. PR16 makes the rename, the built-ins and the user presets, and adds the export command: the export is how a player gets an editable file once the game stops writing them, so it lands in the PR that stops the writing, which is also the first PR with all three built-ins (after PR8) and the stem-named file.
- **design.md** carries this ("Standard Presets", "User Presets", "The tapestry Preset", "Settings Files", "Tuning Without Restart").
- **Why the default is not the tapestry preset:** play uses beatoraja-style by default, as design.md's own introduction describes ("You normally play with 'beatoraja-style'").

## 3. Pull requests

**G0.** Every PR passes, through `mise exec --`:
- `fmt`;
- `clippy -D warnings`;
- `test`;
- `xtask portable test`;
- `check-arch`;
- `check-lints`;
- `cargo deny check`;
- `banned-words`.

**"Done when"** lists only what a PR adds to G0. Each named test appears as `ok`, fails if its change is reverted, and asserts a nonzero count first. In-play code goes into `IN_PLAY`, and its allocation test runs in the dev and release profiles.

### PR1. Preset file loader into `rules-toml` (new crate)

**Touches.**
- New crates/rules-toml, with the loader and its unit tests.
- crates/game/tests/preset.rs stays unchanged.
- The xtask tables.
- design.md "Language and Crate Structure".

**Depends on.** Nothing.

**Done when.**
- When `cargo test -p rules-toml` runs, `preset_file_round_trips_to_equal_canonical_bytes` and `older_preset_file_is_refused_with_the_hint` shall pass.
- When `check-arch` runs, it shall pass.
- When `cargo test -p game --test preset` runs unchanged, it shall pass.

### PR2. Section conditions for `evaluate` (`eval`)

**Touches.** `batch.rs`, `evaluator.rs` and `input.rs`; `game` logic (passes `None`); the generators; design.md "Section Evaluation" and "Testing Strategy".

**Depends on.** Nothing.

**Done when.**
- When the section is `[10 s, 20 s)`, the evaluator shall follow D13's table (`section_boundaries_follow_the_half_open_rule`, `ln_crossing_a_section_edge_is_excluded`).
- If a lead-in press falls outside the first target's early window, then it shall not be judged (`lead_in_press_outside_first_early_window_is_ignored`).
- When a control is held across the start, it shall produce no press edge (`held_across_section_start_is_not_a_press`).
- When the watermark passes the section's final deadline, including an LN end, the evaluation shall be complete (`section_finalizes_through_its_final_deadline`).
- When initial gauges are given, they shall apply (`initial_gauge_is_applied`).
- When 100 of 1,000 notes are perfect from 20 % with `#TOTAL` 160, the normal gauge shall end at 36 % (`section_gauge_uses_the_chart_denominator`).
- When `cargo test -p eval --test props` runs:
  - `section_excerpt_matches_mid_chart_start` shall pass with a nonempty prefix. The prefix leaves held controls, scratch movement and a pending release at the lead-in start.
  - The live/batch and judged-once properties shall generate sections.
  - The coverage test shall count LN cuts, lead-in edge presses and pending-release prefixes.
- When the `eval` allocation test runs, it shall count 0.

### PR3. CPU benchmarks

**Touches.** Benches in `eval`, `bms-parser` and `render`; ci.yml; bench.yml; setup.md.

**Depends on.** Nothing.

**Done when.**
- When `cargo bench -p eval -p bms-parser -- --test` runs on Linux and `cargo bench -p render -- --test` on Windows, each shall exit 0.
- When `bench.yml` runs, it shall print the changes and shall not be a required check.

### PR4. Canonical preset decoder (`rules`, `fuzz`)

**Touches.** canonical.rs; the fuzz target and its seeds; the `targets` list in scripts/fuzz.sh; setup.md.

**Depends on.** Nothing.

**Done when.**
- When canonical bytes are decoded, they shall equal the preset they came from (`canonical_bytes_round_trip`).
- If the version, a count, a tag, the trailing bytes or validation is bad, then decoding shall be refused (`other_canonical_version_is_refused`, `count_beyond_the_bytes_is_refused`, `unknown_tag_is_refused`, `trailing_bytes_are_refused`, `invalid_decoded_preset_is_refused`).
- When the fuzz crate's tests run, the `preset_canonical` seeds shall reach a successful decode and every malformed case.
- When `scripts/fuzz.sh run-all` runs, it shall include `preset_canonical`.

### PR5. Record verification and what-if re-evaluation (`replay`, `game::results`)

**Touches.** `replay::verify`; results.rs and prepare.rs; design.md "Determinism" and "Extension Boundaries".

**Depends on.** PR4.

**Done when.**
- If a check fails, then `verify` shall refuse in D12's order (`refuses_another_evaluator_before_loading`, `old_preset_copy_is_refused_even_with_an_override`, `refuses_another_loader_version`, `refuses_another_resolved_chart`, `refuses_an_unknown_transform`, `edited_file_resolving_the_same_chart_verifies`).
- If both the evaluator version and the preset hash are wrong, then the refusal shall be `PresetHashMismatch` (`decode_checks_precede_verify_checks`).
- When a play is re-evaluated under its own preset, it shall equal live (`as_played_matches_live`).
- When a completed play is re-evaluated under D9's 220 ms preset, it shall complete, clear, and mark "no input" (`what_if_with_a_later_final_deadline_completes_and_clears`).
- When an aborted play is re-evaluated under another preset, it shall stop at the watermark without a clear (`what_if_of_an_aborted_play_stops_at_the_watermark`).

### PR6. `eval-cli` (new crate)

**Depends on.** PR1, PR5.

**Done when.**
- When records are given, eval-cli shall print the as-played and what-if lines and exit 0 (`prints_as_played_and_what_if`).
- If a record is refused or incomplete, then eval-cli shall print `refused: <reason>` and exit 1 (`refuses_with_the_reason_and_exits_one`, `incomplete_record_is_refused`).
- If the arguments are bad, then eval-cli shall exit 2 (`usage_error_exits_two`).
- When eval-cli runs twice on the same input, its output shall be identical (`output_is_deterministic`).

### PR7. beatoraja-style preset, and the default switch

**Touches.**
- `rules`, `rules-toml`, style_beatoraja.rs and the generators.
- crates/game/src/settings.rs: the default `preset` becomes beatoraja-style (D24).
- Existing `game` and `player` tests that relied on the default: they now name the tapestry preset explicitly when they mean it.
- design.md: Non-goals, "The tapestry Preset", "Standard Presets" and "Settings Files".

**Depends on.** PR1.

**Done when.**
- When `cargo test -p eval --test style_beatoraja` runs, every example shall pass and name its source.
- When the preset is written and read through `rules-toml`, it shall round-trip.
- When the props run, the generators shall reach every variant the preset uses.
- When the game starts with no `player.toml`, the first play shall be evaluated and recorded under beatoraja-style (`default_preset_is_beatoraja_style`).
- When `player.toml` has no `preset` key, the play shall use beatoraja-style (`missing_preset_key_takes_the_new_default`).
- PR16 replaces PR7's tests of how `player.toml` names a preset and of missing preset files with the built-in and user-preset tests (D24).

### PR7e. Judgment-header model (`bms-model`, `bms-parser`, `rules`, `eval`)

PR7e lands before PR7a; the letter names it, not its place.

**Touches.**
- `ChartMeta`: the judgment-header declarations replace the resolved rank and `#TOTAL`.
- parse.rs, hash.rs, `LOADER_VERSION` and the loader rules in crates/bms-parser/src/lib.rs.
- The header policy in `rules`: tapestry reads the declarations as loader version 6 does; beatoraja-style reads them by jbms-parser's rules, a `#DEFEXRANK` as a number counting over `#RANK` (F, E). The canonical encoding and file shape.
- `Evaluator::new` reads the rank and `#TOTAL` through the policy.
- style_beatoraja.rs, the generators, and the bms-parser and fuzz seeds.
- design.md "Chart Model" and "What the Loader Reads" (the "not built yet" notes go).

**Depends on.** PR7.

**Done when.**
- When a chart declares `#RANK 3` and then `#DEFEXRANK 120.5`, the loader shall keep both, in order, with their lexical classes (`rank_and_defexrank_are_kept_in_order`), and beatoraja-style shall scale by the `#DEFEXRANK`, 90.375 %, in either order (`a_decimal_defexrank_counts_over_a_rank`).
- When `#DEFEXRANK 120` and `#DEFEXRANK 120.0` are loaded, their declarations shall differ in class and the resolved-chart hashes shall differ (`integer_and_decimal_tokens_stay_distinct`).
- When `#TOTAL 200` is followed by `#TOTAL 0`, beatoraja-style shall use 200, and tapestry shall use its default `#TOTAL` as it does today (`zero_total_after_a_total`).
- When declarations sit in `#IF` blocks, only those of the active branches shall be kept (`inactive_branch_declarations_are_dropped`).
- When only whitespace, the header's case or line positions change, the resolved-chart hash shall stay equal (`header_canonical_form_ignores_text`).
- If a declaration is not a number, then the loader shall warn and keep it as not a number, and the chart shall still load (`unreadable_header_is_kept_and_warned`).
- If a record was made before this PR, then `verify` shall refuse it at the first check that fails in design.md "Determinism"'s order: an unchanged record fails decoding its preset copy, since this PR also bumps the canonical encoding, before the loader version is compared (`record_before_the_header_model_is_refused_at_its_first_failing_check`).
- If a record carries loader version 6 with every other field current (evaluator version and a preset copy in the current encoding), then `verify` shall refuse it with `UnsupportedLoader(6)`, also with a preset override (`refuses_loader_version_6_with_current_fields`).
- When the tapestry preset's existing examples run, they shall pass unchanged.
- When `cargo test -p bms-parser` runs, the property `judgment_headers_keep_order_and_class` shall pass.

### PR7a. Window preparation (`rules`, `eval`)

**Touches.** The lane-class tables (keys and scratch) and their validation; the window policy with beatoraja's scaling; scan envelopes; the deadline plan and the final deadline; the canonical encoding and file shape; style_beatoraja.rs, the generators and the fuzz seeds; design.md "Evaluation Stages".

**Depends on.** PR7e.

**Done when.**
- When a scratch note, a scratch LN end or a scratch empty POOR is judged under beatoraja-style, the scratch table shall apply, and a key note at the same offset shall use the key table (`scratch_uses_its_own_windows`).
- When `#DEFEXRANK` or `#RANK 4` is present, the windows shall scale as beatoraja does, except that a `#DEFEXRANK` percentage is not truncated to an integer (a known difference, `defexrank_scales_by_its_exact_percentage`) (`defexrank_and_rank_4_scale_like_the_source`).
- When a chart declares `#DEFEXRANK n` under beatoraja-style, every edge shall scale by `n × 75 / 100` % as it is, not truncated to an integer percent: `#DEFEXRANK 1` gives PERFECT ±150 µs, recorded as a known difference with the source's zero-width windows (`defexrank_scales_by_its_exact_percentage`).
- When scaled edges tie, preparation shall accept them; if a preset's own table does not ascend, then validation shall refuse it (`derived_edges_may_tie`).
- When a note lies exactly 500 ms ahead of a press under beatoraja-style, judged or not, and the press judges no note, it shall make it an empty POOR whether or not a scaled early BAD edge passes 500 ms, recorded as a known difference with the source's scan bound (`a_note_500_ms_ahead_makes_an_empty_poor_at_every_scale`); at `#DEFEXRANK 292` a scratch press exactly on its early BAD edge shall be BAD (`a_scratch_press_on_its_early_bad_edge_at_219_percent_is_bad`).
- When the scratch table's late BAD edge sets the latest deadline, completion, the what-if's final deadline and the section probe's `final_deadline()` shall all be that time (`final_deadline_has_one_source`).
- When the presets are written and read through `rules-toml`, they shall round-trip; an older encoding shall be refused.
- When the tapestry preset's existing examples run, they shall pass unchanged.

### PR7b. Press resolution (`rules`, `eval`)

**Touches.** Press resolution in `eval` (the candidate scan over judged and unjudged targets by time, apart from the assignment cursor; primary selection with combo priority, B; empty POOR from the scan; emission order); the preset's selection policy; the canonical encoding; style_beatoraja.rs, the generators and the props; design.md "Evaluation Stages".

**Depends on.** PR7a. PR7b and PR7d share one transition contract (design.md "Evaluation Stages", press resolution and LN transitions) and may land as adjacent PRs.

**Done when.**
- When several notes on a lane are in range under beatoraja-style, the press shall judge the note beatoraja's combo-priority algorithm picks, and the earlier notes it passes shall be POOR at the press (`combo_priority_assigns_like_the_source_and_passed_notes_are_poor`).
- When a press judges no note under beatoraja-style, the empty POOR shall be decided from the candidate scan, with the source's ranges for judged and unjudged notes (`empty_poor_is_part_of_the_scan`).
- When a lane's last note has been hit and the lane is pressed again 150 ms after it, the press shall be an empty POOR, under beatoraja-style and tapestry alike (`empty_poor_after_the_last_hit_note`).
- When a press both overtakes notes and judges one, its events shall come in the order sound, overtaken POORs, primary (`press_emission_order_is_fixed`).
- When the tapestry preset runs, assignment shall stay nearest-first, overtaken notes shall be missed at the press, and every existing example shall pass unchanged.
- When the props run, incremental and batch evaluation shall agree under every supported combination, the `Judged` events shall stay within the note count, and the `eval` allocation test shall count 0.

### PR7c. Gauges (`rules`, `eval`)

**Touches.** Gauge preparation (coefficients, counts by purpose) and the accounting order; the preset's gauge fields; mine routing; the canonical encoding; style_beatoraja.rs, examples with Endless Dream's gauge values built in test code (style_lr2.rs, which PR8 extends), and the generators.

**Depends on.** PR7e. It can be developed beside PR7a and PR7b; encoding versions are taken at merge.

**Done when.**
- When the HARD gauge is low, damage shall shrink by the source's bands (`hard_guts_bands_match_the_source`).
- When `#TOTAL` is low or absent, the HARD and EX HARD increase and the default TOTAL shall follow beatoraja's formulas, including a TOTAL below 160 (`total_scaling_matches_the_source`).
- When `#TOTAL` is absent under Endless Dream's gauges, it shall be `160 + (N + clamp(N − 400, 0, 200)) × 0.16` (`lr2_default_total_matches_the_source`).
- When a HARD-family gauge takes damage under Endless Dream's gauges, it shall be multiplied by max(fix1, fix2) (`lr2_damage_correction_matches_the_source`).
- When a change leaves a survival gauge strictly below 2 % under Endless Dream's gauges, a mine's included, it shall drop to 0 and fail; exactly 2 % shall survive (`survival_below_two_percent_fails_including_mines`).
- When a mine damages a gauge with a low-gauge reduction or a `#TOTAL` correction, neither shall apply to the mine, and the clamp and death shall (`mine_damage_skips_judgment_modifiers`).
- When one effect needs the correction, the reduction, rounding and the clamp, they shall apply in the order of design.md "Evaluation Stages", the reduction chosen from the value before the change (`gauge_effect_order_is_fixed`).
- When a section is practiced, N and any damage-correction count shall be the whole chart's.

### PR7d. LN transitions (`rules`, `eval`)

**Touches.** The long-note state machine in `eval` and the eligibility it gives press resolution's primary (the LN-head eligibility variant, K); the preset's policy field; the canonical encoding; style_beatoraja.rs, style_lr2.rs (the preset built in test code), the props (`LnHeld < BAD` in props.rs) and the generators.

**Depends on.** PR7b, whose transition contract it shares.

**Done when.**
- When, under the preset built in test code with combo priority and LN-head eligibility, at 100 %, a lane holds a long note from 0 to 50 ms and a tap at 300 ms, and it is pressed at 160 ms, the primary shall be the tap (BAD, 140 ms early) and the head shall be POOR at the press, before it (`ineligible_head_is_passed_and_poor`).
- When a late BAD press reaches an LN head that the primary does not pass (no primary, or one before the head), the head shall not be the primary and shall be missed at its own deadline, unmoved (`late_bad_ln_head_is_not_selectable`).
- When a long-note head is BAD, under any preset, the long note shall be scored at the press, as today.
- When the props run, every judgeable note shall still be judged exactly once.

### PR8. LR2-style preset (`rules`, `rules-toml`, `eval`)

PR8 assembles policies whose place the earlier PRs and design.md "Evaluation Stages" already fixed: it adds variants to existing stages, not stages. An Endless Dream behavior that needs a new stage boundary changes design.md first.

**Touches.** validate.rs; `rules`: `rules::lr2_style()`, the interpolated and clamped window variant (I) and the empty POOR ranges (L), assembled with PR7b's combo priority (B), PR7d's LN-head eligibility (K) and PR7c's gauge policies (M); `rules-toml`, style_lr2.rs (its examples move from the presets built in test code to `rules::lr2_style()`), the generators, and design.md "LN Judgment Modes" and "Standard Presets".

**Depends on.** PR7a-PR7e. LR2-style chooses its own policy values; an LR2 axis not listed in D11 needs its own failing example.

**Done when.**
- When a release falls within the GOOD width before the end, the head's judgment shall stay; if the release is earlier, then the long note shall be BAD (`release_within_good_width_keeps_head`, `earlier_release_is_bad`).
- When LN-end edges tie, validation shall accept them; if they fall, then validation shall refuse them (`ln_end_edges_may_tie_but_not_fall`).
- When the rank scale is below 100 %, PERFECT, GREAT and GOOD shall follow the interpolated table, and BAD and empty POOR shall stay unscaled (`windows_interpolate_like_the_source`).
- When a chart declares `#DEFEXRANK 400` (300 %), GOOD shall be clamped to BAD's 200 ms rather than scaled to 360 ms, and each of PERFECT, GREAT and GOOD shall stay within the next wider window (`scaled_windows_clamp_to_the_next_wider`).
- When a press judges no note, an unjudged note more than 200 ms and up to 1000 ms ahead, or a judged note 0 to 1000 ms ahead, shall make it an empty POOR, a note exactly 1000 ms ahead included (inclusive here, short of Endless Dream's scan, a known difference), and a note on the late side shall not (`empty_poor_ranges_match_the_source`).
- When PR7d's examples run under `rules::lr2_style()`, they shall pass unchanged.
- When `style_lr2` runs, every example shall pass.
- When a beatoraja-style record is re-evaluated under LR2-style, it shall run to LR2-style's own final deadline (`what_if_under_lr2_style_completes`).

### PR9. The input thread wakes on input; no idle timer (#87)

**Touches.** cadence.rs; win.rs; input_side.rs, window.rs and play.rs; `replay`; every design.md section #87 lists.

**Depends on.** §0 only.

**Done when (portable).**
- `heartbeat_deadline_follows_last_publication`, `input_wakes_never_delay_the_heartbeat`;
- `fast_input_waits_for_the_bound_and_keeps_every_report`;
- `drain_is_cut_and_restamped`;
- `same_us_batch_keeps_the_watermark_rule`;
- `device_change_wakes_like_input`, `focus_change_wakes_an_idle_thread`;
- `idle_starts_only_on_request`;
- `autoplay_stays_timer_driven_until_idle`.

Each one holds the behavior D17 states.

**Done when (Windows).**
- When Raw Input arrives outside a bound interval, it shall be processed without the timer.
- When focus is lost in an autoplay tail without input, the tail shall end (`focus_loss_in_the_tail_without_input_ends_the_tail`).
- When a request is sent to an idle thread, it shall be served (`request_wakes_the_idle_thread`).
- When an idle worker is stopped or dropped, it shall join (`stop_and_drop_join_an_idle_worker`).
- `WakeReport` shall show causes and lateness.

**Done when (records).** Records shall carry the wake policy, and the `input` allocation test shall count 0.

### PR10. Controller menu input

**Touches.** `input` (menu.rs, sources.rs), input_side.rs, select.rs, selection.rs, main.rs, design.md "Input".

**Depends on.** PR9, PR15.

**Done when.**
- While Idle and focused, the bound controller controls shall become menu events (`menu_events_from_bound_controls`).
- While Open or Tail, no menu events shall be sent (`no_menu_events_while_recording`).
- If a packet arrives while the window is in the background, then it shall not reach the menu (`background_packets_never_reach_the_menu`).

### PR11. Japanese text (`render`)

**Touches.** text.rs, the menu pass, the tests, and design.md "Rendering and Skins".

**Depends on.** Nothing.

**Done when.**
- When a Japanese title is drawn with the pinned font, the WARP image shall match within tolerance (`japanese_title_renders_on_warp`).
- If the font hash differs, then the test shall fail and name both hashes.
- When the runtime chooses a font, it shall pick the first present in order (`runtime_font_falls_back_in_order`).
- While a glyph is in a screen's atlas, a later prepare that does not grow the atlas shall not rasterize it again (`glyphs_are_cached_per_screen`).
- When a prepare grows a screen's atlas, the glyphs already drawn shall draw the same afterwards (`atlas_growth_keeps_drawn_glyphs`).

### PR12. Library database and scanning (`library` new crate, `bms-parser`)

**Touches.** Schema 1, `read_header`, benches, the xtask tables, design.md.

**Depends on.** Nothing.

**Done when.**
- `scan_reads_headers_of_new_files`;
- `rescan_reads_only_changed_files`;
- `deleted_files_leave_the_index`;
- `pause_is_acknowledged_after_in_flight_work`;
- `other_schema_version_is_rebuilt`;
- `read_header` resolves no time and no `#RANDOM`.

Each one holds the behavior D5 and D6 state.

### PR13. Search and sort (`library`)

**Touches.** Schema 2, query building, benches.

**Depends on.** PR12.

**Done when.**
- `search_folds_width_and_kana`;
- `search_is_literal_for_punctuation_quotes_and_wildcards`, run with `a-b`, `say "hi"`, `100%`, `a_b` and `ＡＢＣあいう`;
- `short_query_matches_substring`;
- each sort key orders as D7 states.

### PR14. Scene flow (`game`)

**Touches.** app.rs, live.rs, session.rs, policy.rs, the runner, design.md "Play Lifecycle".

**Depends on.** §0 only.

**Done when.**
- `select_play_retry_back_select_again`;
- `refused_chart_returns_to_selection_then_valid_chart_plays` (it calls `entered_selection`);
- `quit_leaves_the_chart_close_ends_the_run`;
- `any_command_sequence_ends_cleanly`, over two built charts and one refused chart:
  - records re-evaluate equal to live;
  - attempts count per chart;
  - every play is reserved before its countdown;
  - every entry into selection is signaled.

### PR15. Song selection on the real threads (`player`, `render`)

**Touches.** args.rs, main.rs, audio_side and audio_loop (`AudioCtl::Load`), render_side (`Load`, `Select`), the select screen, the tests.

**Depends on.** PR11, PR13, PR14.

**Done when.**
- `no_arguments_open_song_selection`;
- `choosing_a_chart_plays_it_and_returns`;
- a second chart plays only its own sounds;
- `refused_chart_shows_its_reason`;
- `refused_load_releases_the_scan_pause`: after a refusal, a newly added chart is discovered without any completed play;
- the pause is acknowledged before each countdown;
- the load time is printed;
- unknown arguments exit 2;
- a second player process started on the same data root is refused before it opens `library.db`, with a message naming the folder (`second_process_is_refused_before_the_library_opens`). `library` already refuses a second `Library` of the file in any process (`LibraryError::AlreadyOpen`, from its lock on `library.db.lock`); the player still decides what a second process does, and today's `tapestry.lock` only decides which process writes the log.

### PR16. Built-in presets, user presets, the shelf and polling (`rules`, `rules-toml`, `game`, `eval-cli`)

**Touches.**
- `rules`: `rules::custom()` becomes `rules::tapestry()` with the ID `tapestry` (custom.rs becomes tapestry.rs, and every caller and test follows), and the list of built-ins by name (D24).
- `rules-toml`: the file loses `id`; the loader takes the name from the file stem and refuses a built-in's name; the refusal hint for an older file points to the export command.
- `game`: settings.rs (`preset` is a name, by default `beatoraja-style`), session.rs (a built-in or a user preset; `standard_preset` and the writing of a missing file go), presets.rs (the shelf of the built-ins and the user preset files, which alone are polled), and PR7's preset-naming tests.
- `eval-cli`: `preset export <built-in> <path>`, and `--preset` taking a built-in's name (D24).
- design.md: "Standard Presets", "User Presets", "The tapestry Preset" and "Settings Files" drop their "not built yet", and "Tuning Without Restart" its "For now" box.

**Depends on.** PR8.

**Done when.**
- When the game starts with no `player.toml` and an empty presets folder, the first play shall be evaluated and recorded under the built-in beatoraja-style, and `presets\` shall stay empty (`built_ins_are_never_written`).
- When `player.toml` names `tapestry`, `beatoraja-style` or `lr2-style`, the play shall use `rules::tapestry()`, `rules::beatoraja_style()` or `rules::lr2_style()` without reading `presets\`, the shelf shall list all three, and saving settings shall keep the name (`built_ins_are_chosen_by_name`).
- When `player.toml` names a user preset, the play shall use `presets\<name>.toml` with the stem as its preset ID, and the shelf shall list it beside the built-ins (`user_preset_is_named_by_its_stem`).
- If a user preset's stem is not a name (uppercase, a dot, another character, or over 64 characters), then loading it shall be refused with an error naming the file (`user_preset_stem_must_be_a_name`).
- If a user preset's stem is a built-in's name, then loading it shall be refused with an error naming the file and the built-in, in the game and in `eval-cli --preset` (`user_preset_named_like_a_built_in_is_refused`).
- If `player.toml` names neither a built-in nor an existing user preset, `custom.toml` included even beside a file `presets\custom.toml.toml`, then loading shall be refused with an error naming the value and the built-ins, and nothing shall be written (`unknown_preset_name_is_refused`).
- If a preset file has an `id` field, then it shall be refused as a file from an older version (`older_preset_file_is_refused_with_the_hint`).
- When `eval-cli preset export beatoraja-style my-rules.toml` runs, it shall write `my-rules.toml`, which loads as `rules::beatoraja_style()` with the ID `my-rules`, and exit 0 (`export_writes_a_user_preset`).
- If the export's stem is a built-in's or not a name, or its file exists, then it shall write nothing and exit 1 (`export_refuses_a_built_in_name_and_an_existing_file`).
- When the chosen user preset's file changes while a result screen shows, the game's result loop shall reload it at the next 250 ms check, re-evaluate the play under it, and hand both to the rig (`changed_user_preset_reloads_within_a_poll`).
- If a changed user preset fails to load, then the previous preset shall stay in use and the error shall be handed to the rig (`failed_reload_keeps_the_previous_preset`); if it then loads again, even to the same rules, the reload shall be handed to the rig too (`a_restored_preset_is_reported_after_a_failure`).
- If a user preset file is a symbolic link, then loading it shall be refused with an error naming the file (`user_preset_links_are_refused`).
- When a user preset changes during a play, that play shall keep the preset it started with, and the next play shall use the reloaded one (`plays_use_reloads_from_the_next_play`).

### PR17. Result screen and preset switching (`player`, `game`)

**Depends on.** PR5, PR15, PR16.

**Done when.**
- `result_switches_presets_and_labels_what_if`;
- `edited_preset_reevaluates_on_the_result_screen`;
- When a user preset file changes while the select screen shows, the shelf shall reload it at the next 250 ms check (`select_screen_polls_the_shelf`);
- When the player returns to the select screen, the shelf shall be kept, not reopened: if the chosen preset's file is then corrupted, the next chart shall play the last valid preset and the error shall be shown (`shelf_survives_returning_to_select`);
- When a poll reloads the chosen preset or finds its file failing, the player's result screen shall show the re-evaluation or the error (`result_screen_shows_reload_and_error`).

**Note.** The shelf reports changes only for the chosen preset (`Shelf::check`); listing every preset on the select and result screens may need a change notice per entry.

### PR18. Personal bests (`library`, `game`, `player`)

**Touches.** Schema 3, `counts_for_best`, the ingest path, the rebuild, the display.

**Depends on.** PR5, PR13, PR17.

**Done when.**
- `completed_full_play_updates_the_best`;
- `played_to_end_with_overflow_sets_no_best`;
- `failed_save_sets_no_best`;
- `only_eligible_plays_set_bests`;
- `random_charts_get_no_best_and_still_save`: two plays resolving different branches set no best and count 2, before and after a rebuild;
- `version_change_invalidates_bests_without_schema_change`;
- `ingest_is_idempotent`;
- `rebuild_from_records_skips_refused`.

### PR19. Scenario launch (dev-tools, CI)

**Depends on.** PR8, PR18.

**Done when.**
- `exit_scenario_plays_select_result_retry_repeatedly`;
- `unrunnable_step_exits_one`;
- `other_scenario_version_is_refused`;
- a production build exits 2 on `--dev-scenario`.

### PR20. Speed in the loader and records (`bms-parser`, `replay`, `game`)

**Touches.** Timing and `LoadOptions`, the 1× identity and scroll map, the speed field, `verify`, the plan, design.md "Chart Model", the hash contract and "Practice Speed Changes".

**Depends on.** PR5.

**Done when.**
- `mapped_times_floor_the_exact_scaled_time` at 50, 65 and 145 %;
- `identity_is_the_same_at_every_speed`;
- property `mapped_chart_keeps_order_and_validates`;
- `windows_stay_in_real_time_at_half_speed`;
- a chart outside ±2^32 µs is refused through D1;
- `speed_play_reevaluates_equal`.

### PR21. Bank at a speed (`audio`)

**Touches.** The speed stage and the workspace preflight.

**Depends on.** Nothing.

**Done when.**
- When the 60 s, 44.1 kHz sound is converted for 48 kHz at 50, 65 and 145 %, the bank shall hold 5,760,000, 4,430,769 and 1,986,206 frames, and its rate shall stay 48,000 (`bank_lengths_follow_the_two_stages`).
- `engine_accepts_a_bank_at_any_speed`.
- When a sound starts at mapped time `t_us`, its start frame shall be within one frame of `t_us × rate / 1,000,000` after the play's origin on the `MixClock` (`starts_align_across_the_ladder`).
- When 383,999 → 384,000 Hz stereo at 145 % is planned, the speed stage shall be 29:20 (input to output) and its bound shall be under 1 MiB (plan only; `speed_stage_workspace_is_bounded`).
- When accepted mono and stereo conversions are constructed and run (for example 8,001 → 8,000 Hz mono, and 44.1 → 48 kHz stereo at each ladder speed), the bytes a counting allocator sees allocated by each stage, at its peak, shall stay at or below that stage's bound (`resampler_workspace_stays_within_its_bound`).
- If a stage's bound exceeds what the cap has left, counting concurrent workers, then the sound shall be refused before construction. The test uses a small cap, and a counting allocator shows that no buffer of the bound's size is allocated (`oversized_resampler_workspace_is_refused_before_construction`).

### PR22. Audio seek (`audio`)

**Depends on.** Nothing.

**Done when.** Property `seek_matches_mixing_from_the_start`.

### PR23. Section rounds (`game`, `replay`)

**Touches.** The plan and rounds, the ring-out and fade, the section probe, the practice record fields (initial gauges by gauge ID), the transfer of initial gauges in `replay::verify`'s what-if, design.md "Mid-Chart Start and Section Repeat", "Practice Records" and "Recording Capacity".

**Depends on.** PR2, PR14, PR20, PR22.

**Why the gauge transfer is here.** Initial gauges enter records only with this PR's practice record fields; a full play starts every gauge at its preset's start, so PR17's what-if of a full play has nothing to transfer. The transfer belongs with the record fields it reads and the `verify` path that builds the target preset's `Section`.

**What it leaves to PR24.** `Loaded::set_practice` sets the rounds up and the headless session restores audio with a seek; the player's rig rings out (`AudioCtl::RingOut`) but restores nothing yet (`Rig::restore_audio`'s default): the seek plan on the play command, the drawing of the section's targets only, and the screen that sets a practice up stay with the practice screen.

**Decided in this PR** (design.md "Mid-Chart Start and Section Repeat", "Practice Records"):
- A record stores the section as evaluated, in real time, with its lead-in; its attempt is the play's number in its session.
- The section is set in 1× time, and both edges are mapped with `floor(t × 20/k)`.
- A mid-chart start is one round, followed by its result screen.
- An autoplay session refuses a practice.
- A preset reloaded at a retry whose gauges no longer take the practice's initial gauges refuses the round at its opening.
- One acceptance rule for initial gauges, `eval::accepts_initial`, serves `Evaluator::new` and the what-if's transfer.

**Done when.**
- When D15's example runs (`section_round_rings_out_and_repeats`):
  - `SongEnd.time` shall be 19.680000 s;
  - completion and the close shall happen in the step at 19.681 s, not at 19.680 s;
  - `RingOut { fade_at: 20.681 s, end_at: 21.181 s }` shall be sent in that step, and the record shall be saved only after the engine reports the ring-out done;
  - the BGM event at 20.5 s and the autoplay key sound of a note at 20.4 s shall start, and the BGM event at 30 s shall never start;
  - round 2's countdown shall begin at 21.182 s, with the long voice 3 s in, also when the engine reports done early with two or three queued periods (`early_done_report_waits_for_end_at`).
- When retry is pressed during the ring-out, the ring-out shall be cut in that step and the frozen record shall still be saved (`retry_cuts_the_ring_out_and_saves`).
- When a slow save is simulated, the ring-out's fade, stop and quit handling shall happen at the same times as with an instant save (`saving_never_delays_the_ring_out`).
- When the engine renders a ring-out, the gain shall be 1 before `fade_at`, fall linearly per frame to 0 at `end_at` (24,000 frames at 48 kHz in D15's example), and every voice shall stop at `end_at` (`ring_out_envelope_is_on_output_frames`, `audio`).
- When the same ring-out is rendered with different period sizes, including one not dividing the fade (e.g. 7 ms and 10 ms), the output shall be identical sample for sample (`ring_out_is_independent_of_period_size`, `audio`).
- When a play starts after a completed ring-out and after one cut mid-fade, its output shall be at full gain from its first frame (`play_resets_the_ring_out_envelope`, `audio`).
- When the ring-out is rendered, the audio callback shall allocate nothing (`ring_out_allocates_nothing`, `audio`).
- `section_round_records_its_conditions`.
- When a practice record made under tapestry with `normal` at 50 % and `survival` at 70 % is re-evaluated under beatoraja-style, `normal` shall start at 50 %, every other gauge at beatoraja-style's start value, and the result shall mark those as started at the default (`what_if_transfers_initial_gauges_by_id`).
- If a recorded initial gauge lies outside the target gauge's bounds (a test preset whose `normal` starts at 20 % with bounds 10-100 %, and a recorded `normal` at 5 %), or is 0 on a survival gauge, then that gauge shall start at its preset's start value and be marked like a missing one (`out_of_bounds_initial_gauge_uses_the_default`).
- When the windows are computed, they shall be 21.180, 16.795384, 11.680 and 8.513333 s (`practice_window_is_sized_by_the_section`).
- When the long-note section is planned, its deadline shall be 19.900 s, its window 11.900 s and its capacity 12 s (`practice_window_includes_ln_end_deadlines`).
- `excerpt_matches_mid_chart_start_end_to_end`.
- `slow_full_play_over_budget_is_refused`.

### PR24. Practice screen and start position (`player`)

**Depends on.** PR19, PR21, PR23.

**Done when.**
- `practice_screen_repeats_and_quits`;
- `--dev-start <ms>` starts the play at that time;
- the exit scenario's practice round passes.

### PR25. Loudness normalization (`audio`, `library`, `player`)

**Touches.** `Mixer::mix_raw`, the block job with histogram mode, schema 4, the gain, design.md "Algorithms".

**Depends on.** PR15, PR18.

**Done when.**
- When the −12 and −30 LUFS examples are analyzed, the gains shall be 0.501 and 1.995 (`gains_follow_the_formula_and_caps`).
- `overlapping_full_scale_sounds_measure_above_full_scale`.
- `analysis_ignores_master_volume`.
- When 1-minute and 10-minute charts are analyzed, the job's total allocated bytes shall be equal, including the analyzer (`analysis_memory_is_independent_of_duration`).
- `histogram_mode_keeps_whole_chart_loudness`.
- `replacing_a_sound_file_invalidates_the_analysis`.
- While a play runs, the job shall be paused, and judgment shall be unchanged.

### PR26. The key sound of a press (`eval`)

**Touches.** Press resolution's sound choice in evaluator.rs (the scan names the note an empty POOR is against, the nearest inside the range, and the lane's latest target before the press); the examples in crates/eval/tests/sound.rs; the event-stream digests in crates/game/tests/loaded_eval.rs, whose non-sound events are unchanged; the evaluator version (`Implementation::STANDARD_VERSION` 3), since the event stream is the evaluator's output; design.md "The Only Output Is the Judgment Event Stream" and "Evaluation Stages".

**Depends on.** PR7d, which also changes press resolution.

**Done when.**
- When a press judges a note, it shall play that note's sound, under every preset (`a_judged_press_plays_its_note`).
- When a press makes an empty POOR against a note, it shall play that note's sound (`an_empty_poor_plays_the_note_it_is_against`).
- When a press judges nothing between notes, it shall play the lane's latest earlier note's sound, judged or not; and before the lane's first note, that note's sound (`a_press_on_nothing_plays_the_latest_earlier_note`).
- When the same input is evaluated under the tapestry and beatoraja-style presets and a press judges nothing under both, the sound shall be the same (`the_fallback_sound_is_the_same_under_every_preset`).
- When a press in a practiced section's lead-in (design.md "Section Evaluation") judges nothing, it shall still play no sound, as today (`lead_in_press_outside_first_early_window_is_ignored`).

### Order and parallel work

**Start in parallel:** PR1, PR2, PR3, PR4, PR9, PR11, PR12, PR14, PR21 and PR22.

**Then:**
- PR1 → PR7 → PR7e → PR7a → PR7b → PR7d → PR8 → PR16
- PR7e → PR7c → PR8
- PR7a–PR7e → PR18 (bests key on the preset bytes and the interpretation tuple, so the standard presets and the loader version settle first)
- PR4 → PR5
- PR1 + PR5 → PR6
- PR12 → PR13
- PR11 + PR13 + PR14 → PR15
- PR9 + PR15 → PR10
- PR5 + PR15 + PR16 → PR17
- PR5 + PR13 + PR17 → PR18
- PR8 + PR18 → PR19
- PR5 → PR20
- PR2 + PR14 + PR20 + PR22 → PR23
- PR19 + PR21 + PR23 → PR24
- PR15 + PR18 → PR25
- PR7d → PR26

**Format numbering.** Library schema numbers follow the order PR12 < PR13 < PR18 < PR25. Replay format bumps (PR9, PR20, PR23), canonical encoding bumps (PR7e, PR7a–PR7d, PR8) and the loader version bump (PR7e) take the next number at merge.

## 4. Verifying the stage exit

### Automated

**Exit scenario** (Windows CI, dev-tools; PR19, extended by PR24). The data root starts with no `player.toml`.
1. Scan two generated charts and one refused chart.
2. Choose the refused chart and see its reason.
3. Add a chart and see it listed.
4. Play A with autoplay; it is evaluated under beatoraja-style, the default.
5. On the result screen, switch to tapestry, then LR2-style, then back.
6. Retry with a scripted log to the end, then retry and abort.
7. Go back, play B, go back.
8. Play one practice round at 75 %, then quit.

**It asserts:**
- the scene order;
- the default preset;
- every record re-evaluates equal;
- what-if results are labeled and not empty;
- bests come only from the eligible play;
- zero render-app allocations.

**Headless on Linux:** PR14's flow tests and property, and PR7's default-preset tests.

### Property tests that must pass

**Existing:**
- in `eval` (crates/eval/tests/props.rs): `every_judgeable_note_is_judged_once`, `holding_as_long_as_the_long_note_keeps_the_head_judgment`, `live_matches_batch_for_every_delivery`, `recording_interleaving_does_not_matter`, `merge_orders_by_time_then_source`, `gauges_stay_in_range_and_survival_failure_is_permanent`, `widening_the_windows_never_worsens_a_judgment`, and `generators_reach_the_fragile_spots`;
- in `replay`: the round trip and `generator_reaches_ten_byte_varints_and_empty_lists`;
- in `bms-model`: `position_at_brackets_the_exact_curve`.

**New or extended:**
- `section_excerpt_matches_mid_chart_start`, plus section coverage in the existing properties (PR2);
- the standard-preset generators, drawing every supported policy combination (PR7, PR7a–PR7e, PR8);
- `judgment_headers_keep_order_and_class` (PR7e);
- `canonical_bytes_round_trip` (PR4);
- round trips of the new record fields (PR9, PR20, PR23);
- `any_command_sequence_ends_cleanly` (PR14);
- `mapped_chart_keeps_order_and_validates` (PR20);
- `seek_matches_mixing_from_the_start` (PR22).

`preset_canonical` runs in the short and daily fuzz runs.

### Developer checklist (development PC)

1. **Library.** Set `[library] roots` and start `tapestry`. The library fills, Japanese titles read correctly, and search works by kana and by a romanized word present in some chart's metadata.
2. **Default preset.** On a fresh data root, plays use beatoraja-style and `presets\` stays empty. A `player.toml` naming `tapestry` plays tapestry, and one naming `custom.toml` is refused with an error naming it.
3. **The loop.**
   - Choose a chart; play starts within 1-2 s.
   - Play to the end, step through the three presets, retry several times, go back and change charts. Do this with both keyboard and controller.
   - A refused chart shows why, and new charts still appear.
4. **Preset reload.** Export LR2-style as a user preset (`eval-cli preset export lr2-style %LOCALAPPDATA%\tapestry\presets\my-rules.toml`), choose it, and edit `presets\my-rules.toml` while it is shown; it updates at the next poll.
5. **Practice.** Practice a section at 75 % with repeat. After the section is judged the BGM plays on briefly without notes and fades out, the next round starts, and long BGM is already playing at its start.
6. **eval-cli.** Run it over saved records with two presets, and see a refusal for a record from before a version bump.
7. **Idle input thread.** It barely wakes on menus, and the play overlay figures are unchanged.

## 5. Risks that change the plan

- **glyphon falls behind a wgpu update the workspace needs.** The text module moves to cosmic-text glyphs in our own atlas.
- **A style rule does not fit a stage's policy.** That preset gets its own evaluation implementation (design.md "Policies, Combinations and Versions").
- **Input wakes worsen the play figures.** The in-play cadence goes back to timer-only.
- **The two-stage conversion is audible, or a reload at a new speed is slow.** Use the single exact stage where the preflight admits it, or add a converted-PCM disk cache.
- **SQLite stalls selection.** Queries move to a reader thread.
