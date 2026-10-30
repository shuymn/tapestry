# BMS Player Basic Design

The basic design of a Windows-only BMS player (a 7-key + scratch music game). Centered on an evaluator that can recompute judgments from the input log, it defines the boundaries out to rule presets, skin compatibility, a custom IR, and chart distribution.

Target: Windows 10/11 / Time unit: integer µs / Core language: Rust (recommended)

## Goals and Scope

Build a BMS player that plays like beatoraja or Lunatic Rave 2. Its biggest distinguishing feature is that every input during play is recorded with a timestamp, so judgments, score, and gauge can be recomputed later under a different rule. You normally play with "beatoraja-style", and on the result screen you can switch over to see "how it would have gone under LR2-style".

### Goals

- A game that feels good to press (key sounds, judgment display, frame stability) and makes you want to replay it again and again
- Running alongside streaming or recording software without significantly impairing their operation
- Fair judgment based on input timestamps, robust enough for play that competes over a single point of score
- Judgment, gauge, and scoring rules switchable via presets (data)
- The same input log and the same preset produce the same result on any environment
- Existing skin assets (LR2 CSV, beatoraja JSON and Lua) usable through adapters

### Non-goals

- **Exactly reproducing the judgment of beatoraja or LR2.** We provide "-style" presets with similar behavior, but do not go as far as matching bugs in the existing implementations.
- **Making the judgment of the commercial game this is based on the standard.** The default is a custom preset.
- **Importing or converting commercial game data.** It goes neither in core nor in official extensions.
- Support for platforms other than Windows.

## Design Principles

> **Evaluation is a pure function**
>
> Judgment, score, and gauge are determined solely by `(chart, input log, preset)`. Evaluation does not read the clock, audio, rendering, or global state, and does not modify the chart.

> **Input is every edge, timestamped**
>
> Every press and release, without exception, is appended along with the QPC time at the moment it was received. Never keep only the "current state" of the keys.

> **Time is integer µs**
>
> The clock is the monotonically increasing QPC. Chart, input, and judgment all use `i64` µs, and the gauge is computed in fixed point.

> **Core is closed; extend outside it**
>
> The evaluator has no plug-in points. Extensions are placed only before evaluation (preparing the chart and input) and after it (receiving judgment events).

Live play, results, replays, and the IR server all use the same evaluator code. This structurally eliminates any room for the in-play display and the numbers on the result screen to diverge.

> **Prohibited: do not write the name of the commercial game this is based on**
>
> Do not put the title, series name, maker name, or online service name of the commercial game this is based on into code (identifiers, comments, strings, log messages, test names, asset or file names). Use generic terms such as "7-key", "scratch", and "BMS". Matching against the banned-word list is done by calling the same check script from both the git pre-commit hook and CI. The hook is for noticing locally before a commit and can also be skipped locally, so the final gate that fails on any hit is CI. Matching targets the self-authored code, file names, and documents managed in the repository, normalizes case and Unicode, and is done at word boundaries. Only the dedicated file that holds the banned-word list is exempt; external dependencies and license documents are out of scope. Commit messages are out of scope. The list holds only these names and their common spellings, not general game terms, so it never blocks ordinary development. Periodic reviews of the tree and history catch other problem expressions; each fix adds the missed term to the list.

Strictness is required only around judgment, time, and the input log. Everything else (screens, effects, skins, settings screens) is built lightly, on the assumption that it will be reworked while checking it by playing.

## Game Experience

This is a game, and the correctness of judgment is merely the foundation for "being pleasant to play". The quality of the design is judged by whether it actually feels good when played.

### Feel Targets

| Item | Target | Reason |
| --- | --- | --- |
| From press to key sound | 10 ms or less on the low-latency path; shared mode follows the device period ("Path from Press to Sound") | Audio latency is felt as "heavy" before judgment deviation is |
| From press to judgment and bomb appearing on screen | 1–2 frames | The feel of a press is determined by the screen's response |
| Frame stability | No drops at 144/240 Hz, watched through the frame pacing indicators ("Frame Pacing in Refreshes") | Even a momentary hitch in the note flow breaks reading |
| From song selection to play start | 1–2 seconds | It is a game you replay many times, so waiting time turns directly into frustration |
| Retry | One action, instant | Repeating the same chart is the core of play |

### Gameplay Loop

- **Song selection**: Search and sorting that narrow down even a library of tens of thousands of songs instantly, selection by difficulty table, and song preview playback. Library scanning runs in the background and processes only the differences.
- **Play**: Judgment text, combo, bombs, lasers, and gauge movement respond pleasantly the instant you press. Effects are built by receiving the judgment event stream, so they can be elaborated freely.
- **Result**: The score and the difference from the personal best, the gauge history, the early/late distribution, and the results under other presets can be seen on one screen. Show information that makes you want to play one more time.
- **Practice**: Section repeat, speed change, and offset adjustment (a recommended value is derived from the input log). Improvement itself makes up most of the play, so practice features are not postponed.

### How We Build It

- The first target is a state where "one song can be played pleasantly to the end". IR, distribution, and skin compatibility are added after that.
- Effects, screens, and the default judgment and gauge values are treated as things tuned by playing. Tuning can be tried without restarting, by reloading presets and skins.
- The default look (custom skin) and the default judgment are the face of this game. Make the standard setup fun to play without relying on compatible skins.
- Build it to completion with the fewest extensions first. Every extension point exists in code from the start, but only the extensions the first playable version needs are plugged into them ("Extension Boundaries"). Variations (other PCs and refresh rates, running alongside other software, skins, presets) are then added one at a time. If a design flaw blocks one of them, the affected part is rebuilt from scratch with that flaw taken into account. This is a personal project, so finishing the concept matters more than the effort it takes.
- Performance comes from doing what game development normally does (the "Performance" section): no wasted resources, and no over-engineering. Measured figures are tracked to find problems; they are not targets that stop work.

## Overall Architecture

*Figure: The input thread appends to the input log, and the evaluator on the logic thread, together with the rule preset, evaluates up to the input watermark and emits the judgment event stream. The audio thread, the render thread, and results and replays receive it; rendering goes through the skin adapter.*

| Thread | Role | Must not |
| --- | --- | --- |
| Input | Receives device events, attaches QPC times, and appends them to the input log. Publishes the watermark | Judgment, processing that allocates memory, console output (its messages travel with its replies and the logic thread prints them) |
| Logic | Wakes on input publication and watermark updates, and advances the evaluator up to the watermark. Passes judgment events to audio and rendering | Waiting on rendering or file I/O |
| Audio | Mixes in the WASAPI callback. Plays key sounds and BGM | Acquiring locks, allocating memory |
| Render | Reads the state snapshot and draws the skin | Writing to the evaluator or the chart |
| Loading | Loading charts, audio, and BGA, and decoding video | Heavy processing during play (finish it beforehand) |

Threads are connected by lock-free single-producer single-consumer queues. One queue is placed per input source, one for logic→audio, and one for logic→render (effect events), and all data that must not be dropped is passed through queues. Triple buffering is used only for the render state snapshot, where only the latest value is needed. There is no shared mutable state.

- The input thread runs on a 1 kHz high-resolution waitable timer under MMCSS "Games", and the audio thread on the WASAPI event under MMCSS "Pro Audio". The render thread waits on the swap chain's waitable object, a dedicated window thread runs the winit event loop (created with `with_any_thread`) while the game keeps the main thread, and loading uses a bounded pool that ends before the countdown.
- The queues are `rtrb` and the snapshot is `triple_buffer`, both proven allocation-free on every hot path in stage 0. Our own ring of atomics would be code to own and verify for no gain, and `crossbeam-channel` is an MPMC queue with heavier synchronization than one producer needs.
- The input thread wakes the logic thread with `Thread::unpark` after each publication, and the logic thread waits with `park_timeout`. This is portable, allocation-free and survives a missed wake ("What Wakes the Logic Thread"). A Win32 event would make `game` Windows-only.

### Mechanisms for Testing

The following three would mean rework if added later, so they are built in from the start.

- **Swappable clock**: QPC is not read directly but through `TimeSource`. Production uses the real QPC; tests use a virtual clock that the test advances itself. Input, logic, audio, and rendering all read time through it.
- **Processing that can advance without threads**: Logic processing is written as a function that "advances to a specified time", and threads only call it. Tests call the same function sequentially, without threads.
- **Swappable I/O**: Input can be replaced with fake input fed from an input log or a test scenario. Audio can be replaced with an output that generates waveforms offscreen, or an output that records scheduled sounds and sample positions. Rendering can draw to an offscreen texture.

In tests, a test runner advances the whole system in the same order as production: feed fake input → publish the per-source watermarks → advance the evaluator up to the watermark → process one audio block → render. The evaluator is never advanced directly, bypassing the watermark. The runner also simulates the audio playback position and its QPC time, injecting clock drift, read jitter, and discontinuities from resume, to verify the same audio clock correction as production.

## Time Base (QPC)

The correctness of this design depends on aligning all times to a single clock. The base is `QueryPerformanceCounter` (QPC), and all three clocks (input, audio, and display) are mapped to QPC. Wall clocks (`system_clock`, etc.) are not used.

### Handling QPC

- QPC is consistent across all cores and never goes backward. CPU affinity is not pinned. Times taken on different threads cannot be ordered when they differ by ±1 tick, so the order of events is determined by a total order of time and sequence number.
- The frequency (`QueryPerformanceFrequency`) is read once at startup. On many environments it is 10 MHz (100 ns ticks). Conversion to µs multiplies first and then divides, in `i128`. Rounding is fixed to mathematical floor (toward the smaller side even for negative values; `div_euclid` in Rust).
- The QPC value at play start is the chart's 0 µs, and every later time is expressed as the elapsed µs from there. Only relative times are written to the input log, and raw QPC values never leave the PC (because they reveal the time since boot).
- QPC keeps advancing during sleep. When the game window receives the suspend and resume notifications (`PBT_APMSUSPEND` of `WM_POWERBROADCAST` and the resume notification; on Modern Standby environments, also register for the notifications), that play is aborted and the input log is marked "incomplete". Other power notifications, such as a change in AC power connection, do not abort play.

### Timestamps Used for Judgment

Judgment uses the QPC taken immediately after the input thread receives the event (the receive time). The watermark guarantee (the "Input" section) holds on receive times. Keyboards and dedicated controllers are received via Raw Input and MIDI via callbacks, and all of them are treated the same.

GameInput input carries µs timestamps that can be compared with `IGameInput::GetCurrentTimeStamp`. However, the official specification does not tell where that timestamp is attached, its exact correspondence with QPC, or whether the clocks drift relative to each other. Also, input read out later can carry a time earlier than an already-finalized watermark, so using it for judgment as is would break the watermark guarantee. For now, it is therefore handled as follows.

- GameInput timestamps are recorded in the input log alongside the receive times and used only for measurement (the variability of the latency until receipt). The mapping to QPC is obtained by repeatedly measuring `GetCurrentTimeStamp` reads bracketed by QPC reads, and its uncertainty is also recorded.
- Judgment switches to GameInput timestamps only if measurement confirms that the latency until receipt has an upper bound. In that case the watermark is "the receive-time watermark minus that bound", and any input delayed beyond the bound marks the input log "incomplete".

### Mapping the Three Clocks

| Clock | How it is mapped to QPC | Used for |
| --- | --- | --- |
| Input | The QPC immediately after receipt (previous subsection) | Judgment |
| Audio | At each buffer event, the first frame still queued (frames written minus padding) and the QPC time of the engine pass that reads it (the wake plus one period). The least-late wake of a sliding window anchors the mapping, and it is re-anchored on every period. `IAudioClock::GetPosition` is recorded as a diagnostic only | Scheduling sound starts on the judgment timeline (the "Time and Offsets" section) |
| Display | The QPC time of a past vertical sync (`SyncQPCTime`) in the DXGI frame statistics (`GetFrameStatistics`), and the corresponding display-refresh count | Aiding the prediction of note draw positions, and measuring display latency |

- DXGI frame statistics are only an aid to prediction. The first time, right after a display mode change, when the statistics are stale, and when the refresh period is irregular under VRR, prediction uses only QPC and the configured display latency.
- The display's lag and the scan-out time down to the judgment line are calibrated as the display latency offset. Do not double-add the portion derived from the statistics and the offset.
- wgpu does not expose the raw swap chain, so the DXGI swap chain is reached from inside the DX12 backend (`as_hal`). This part is confined to `render`.

### What Determines Timing Accuracy

What determines the fairness of judgment is not the resolution of QPC, but the latency from the press until the receive time is attached, and its variability.

- **USB polling**: With a 125 Hz device, waiting for the USB report alone introduces a spread of up to 8 ms (1 ms at 1000 Hz). Report intervals are recorded, and the polling rate is estimated only when the conditions for a report to arrive on every change are met. If it can be estimated and the rate is low, a warning is shown; if it cannot be estimated, "not estimable" is recorded.
- **Latency until the OS delivers**: The roughly constant part is corrected by the input latency offset.
- **Variability in thread wake-up**: The time is attached immediately on waking, and events are read in batches with `GetRawInputBuffer`. The input thread is prioritized with MMCSS. Events read in one batch get the same receive time, and their individual occurrence times cannot be recovered.

The receive method (Raw Input, MIDI, etc.), the record of report intervals, and the estimated polling rate (or "not estimable") are recorded in the input log as device information. The measurement display shows the distribution of report intervals, the variability in thread wake-up, and the deviation in the mapping of the three clocks, so the timing quality of that environment can be checked in numbers.

## Time and Offsets

- **Judgment timeline**: The elapsed µs from play start, based on QPC (the "Time Base (QPC)" section).
- **Unit**: Chart, input, and judgment times are all `i64` µs. There is no rounding to ms.
- **Chart time**: Time computation, including BPM changes and STOPs, is done with rational numbers, and rounding to µs happens only once. This eliminates accumulated floating-point error and environment-dependent differences.
- **What is rounded**: Only values compared with a clock are floored to µs, once, from the exact value: the judgment times of notes and long-note ends, sound start times (BGM and key sounds), and the playhead of a frame, read at the frame's µs and shared by everything on screen. Object identity and order (which objects are the same, which survives a conflict, long-note pairing, a note inside a long note) and display geometry (object positions, scroll-map anchors and STOP extents, culling) come from the exact source position and exact time, never from floored µs. Objects at distinct positions stay distinct even when they floor to one µs, so one lane may hold several notes at one µs, a long note may start and end at one µs, and a note may start at the µs where the previous long note ends. Judgment needs no rule of its own for them: the existing note assignment takes them one press each, nearest first and earlier on a tie ("Algorithms").

### Aligning Audio to the Judgment Timeline

The audio device's clock drifts gradually relative to QPC. The judgment timeline is fixed to QPC, and the drift is absorbed on the audio side. This way, judgments can be reproduced without keeping an audio clock conversion history in the replay.

- The mapping comes from the buffer events, not from `IAudioClock::GetPosition`. On a tested interface, `GetPosition` advanced a whole period at a time against a current QPC, so its position is recorded only as a diagnostic. At each event, the first frame still queued is read by the next engine pass, one period after the wake. Wakes come late by a scheduler-dependent amount and engine passes do not, so over a sliding window the least-late wake anchors the mapping between frames and the judgment timeline. It is re-anchored on every period, which follows the device clock's drift against QPC.
- Every sound start is scheduled through this mapping: a key sound at the first frame not yet written (two queued periods put it about 2–3 periods after the command is received), and a BGM event at the frame its chart time maps to, minus the audio output latency. A start whose frame is already written moves to the first frame not yet written; the head of a sound is never cut.
- Re-anchoring keeps every sound start on the QPC timeline without resampling during play. Drift then shows only inside one long, already running sound (about 9 ms over 3 minutes at 50 ppm). The measured drift (ppm) is recorded per play. The mixer resamples to correct drift (the "Audio clock correction" row in "Key Libraries and Algorithms") only once a chart whose single-file BGM is heard drifting against key sounds, or a recorded drift that would reach a display refresh over a song, calls for it.
- The mapping is re-established on start, on an underrun (a wake that finds the buffer empty), and on a jump such as resume.

### Three Separate Offsets

| Name | Meaning | Where used |
| --- | --- | --- |
| Input latency | The delay from the press until the input thread receives it | Evaluator (subtracted from the receive time to get the evaluation time) |
| Audio output latency | The remaining delay from the device playback position until the sound actually reaches the ear | Audio scheduling position |
| Display latency | The delay from rendering until it appears on screen | Note draw position only |

The `GetPosition` position represents the position the device has already played. Therefore, the delay already included in it must not be double-added as audio output latency. Audio output latency represents only what remains in measurement.

The three offsets are recorded in the replay as inputs to evaluation.

### Automatic Offset Adjustment

- **Suggestion (always)**: After play, a recommended value is derived from the input log. It is produced even when adjustment is disabled.
- **In-play adjustment (optional)**: When enabled by the session policy, the input latency offset is moved gradually during play. The adjustment rules (range of observed deviations, moving-average window, update interval, allowed range, maximum rate of change) are held as a versioned session policy.
- **Recording**: The offset changes actually applied are kept in the input log. Which offset you played with is treated as part of the input, just like how you pressed.
- **When recomputing**: The adjustment rules are not rerun; the recorded changes are applied as is. Whichever preset is used for recomputation, judgment uses the same offsets as during play.
- **Verification**: The adjustment rules are determined solely by the judgment deviations up to that point. Recomputing with the preset and rules used during play should reproduce the recorded changes, so the IR checks this and rejects records that do not match.

### Offset Changes and the Watermark

Offsets are expressed per source as a function of receive time, `offset_i(t)`, and the evaluation time is `t − offset_i(t)`.

- Changes do not jump at once; they move continuously as a piecewise linear function with an upper bound (less than 1) on the slope with respect to receive time. Values are integer µs, and interpolation within a segment is computed with a fixed rounding method (floor).
- A new change is added only as a segment that starts at a receive time later than the point at which it was decided. Segments up to the published watermark are never rewritten.
- Adjustment uses only finalized judgments as its material, and when the next segment starts is decided by a rule fixed by the evaluator version.

This keeps the mapping from receive time to evaluation time always monotonically increasing, and past mappings never change, so an input with an evaluation time earlier than the finalized watermark never shows up later.

In-play adjustment works in the direction of shrinking judgment deviation, so it is treated as a kind of assist feature. For now, the IR separates records with adjustment enabled from the normal rankings.

Offset changes during play are not built yet: the input log has no offset change segments, so the input latency offset changes only between plays. Display latency and volume change at any time, since they do not affect judgment.

## Chart Loading

The loader (`bms-parser`) turns chart bytes into a resolved chart: judgment times in integer µs and display positions in fractions of a beat. The rules of the current loader version (which channels and headers it reads, how conflicts resolve, what warns, what is refused) are listed in the crate documentation (`crates/bms-parser/src/lib.rs`), and changing any of them bumps `LOADER_VERSION`, which every record carries ("Determinism").

### Chart Model

- The chart is fully resolved: per-lane note vectors sorted by time (lane 0 scratch, 1–7 keys), with `Tap`, `Long { end, end_pos, end_sound }` and `Mine { damage }`, each head and long-note end holding its time in µs and its display position; BGM events; the `#WAV` table (the name as written and the path resolved inside the chart folder); bar lines as display positions (never compared with a clock); the scroll map; metadata (title, `#RANK`/`#DEFEXRANK`, `#TOTAL` in thousandths, the chart's LN mode); and the identity (SHA-256 and MD5 of the file bytes, the resolved-chart hash, the loader version, the `#RANDOM` results).
- Per lane, because the evaluator keeps a cursor per lane and rendering finds the visible range per lane. One time-sorted list in measure positions would make every consumer redo the rational conversion, and the evaluator would see notation instead of time.
- Within a lane, times and display positions never decrease: a note starts no earlier than the previous one ends, a long note ends no earlier than it starts, and its end position is not below its head's. Equal times and equal positions are allowed, since distinct source positions may floor to one µs and positions closer than 2^-80 beat to one position. Bar-line positions never decrease. `Chart::validate` checks this for the loader's output, charts built in code and the output of every chart transform alike, so the renderer can cull by position.
- The resolved-chart hash covers the resolved objects with their display positions, the scroll map and the sound names as written, never resolved paths or the `#RANDOM` results that chose them, so moving a chart folder keeps its identity and two draws that resolve alike share one hash.
- `bms-model` uses no floating point. A display position is an `i128` count of 2^-80 beat, the scroll map is a piecewise linear function from integer µs to positions (a STOP segment has slope 0), and the renderer converts position differences to pixels in floating point on its own side.

### Chart Time

- Time is accumulated per segment between BPM changes and STOPs as an exact rational of arbitrary-precision integers (`Ratio<BigInt>`), and each object's time is floored to µs once and narrowed to `i64` with `try_from`. The value is exact, so it equals what any other exact representation gives. A STOP's length in judgment time is exact too; the scroll map samples the exact curve at whole µs ("Display position" below), so a STOP's extent there follows from its sampled boundaries.
- **Display position.** An object's display position is its exact beat count (the spans of the earlier measures plus `index / count` of its own measure's span), floored once to 2^-80 beat. It is computed per position from small rationals (its reduced denominator is below 2^83) and never touches the running time; BPM changes and STOPs add no beats. The scroll map, which gives the playhead, has one segment per BPM change, STOP start and STOP end. A segment starts at `ceil(T)`, the first whole µs at or after its exact time `T`, with the anchor `floor(2^80 × P(ceil T))` on the exact curve `P` (beats over time) and the slope `floor(2^80 × r)` for the exact rate `r` in beats per µs (0 during a STOP). Changes whose starts are equal coalesce in exact event order, the later replacing the earlier, so same-position precedence stays the timeline's. The map is read only at integer µs: a segment covers queries up to one before the next start and stays at or below the exact curve there, so it reaches the next anchor without passing it. The playhead therefore never decreases, a section or STOP shorter than 1 µs still shifts what follows through the next anchor, and the error since the last anchor stays below one unit per µs, 2^-48 beat over the whole domain. Before time 0 the map runs at the initial rate of the `#BPM` header, kept apart from the segments, since the first segment may be flat (a STOP at time 0); there the error is likewise at most one unit per µs, above the exact curve.
- **Position range.** A BPM below 10^12 advances less than 2^14.03 beats per µs, so over ±2^32 µs positions stay below 2^46.03 beats, 2^126.03 units, and every anchor and every slope × Δt fits `i128`. Eighty fractional bits keep the finest scale (0.000001 BPM shown over a 100 ms visible time on a 2160-pixel lane, about 2^39.8 pixels per beat) within 2^-8 pixel: a floored slope loses at most 2^-48 beat over 2^32 µs. The map is defined on ±2^32 µs and clamps a query outside it. Compiling it checks starts against the domain, and anchors, cross-segment steps and end-of-domain products with checked arithmetic; conversions split quotient and remainder so no intermediate overflows, and the parser keeps rationals wide until a checked narrowing. Only retained positions and anchors are narrowed, so measures past the chart's end, which are dropped, never refuse it. Out of range is a load error naming the measure; accepted charts cannot reach it, and with raised limits it refuses retained times beyond 2^32 µs.
- Decimal values (BPM, measure length, `#STOP`, `#TOTAL`, `#DEFEXRANK`) are parsed from the trimmed text straight into rationals. The text must match `[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?`; commas, inner whitespace, `NaN`, `Infinity` and hex floats are not numbers. Counting digits without the zeros that do not change the value, a value has at most 12 integer digits and a positive exponent at most 18; digits past the 18th fractional one are cut off toward zero, and a negative exponent only moves digits past that point (`1E-100` is 0), so it may be larger but is at most 18 digits long (its own leading zeros do not count). Every bound is checked on these counts before any number is built, so a long run of digits costs a scan and nothing more, and every value is `n / 10^18` with `|n| < 10^30`.
- A BPM below 0.000001 (`≤ 0` included) and a measure length `≤ 0` are load errors: reverse scroll, which some charts use on purpose, is a later feature, and a measure without length has no timing. Time and display positions use the exact BPM.
- **Size cap.** The running time is the only value that grows with the chart: its reduced denominator divides the least common multiple of every segment's denominator, so distinct BPMs multiply in (20 distinct odd prime BPMs already pass 2^128). The text limits bound every other value: a position inside a measure is `index / count`, and a line has fewer than 2^23 slots in a 16 MiB file, so the span between two positions of one measure has a denominator below 2^46; measure lengths and BPMs are `n / 10^18` (10^18 < 2^60, `n` < 2^100), so a segment's µs, `Δbeats × 60,000,000 / BPM`, has a denominator below 2^46 × 2^60 × 2^100 = 2^206, and a STOP's below 2^166. The time's denominator is capped at 16384 bits, checked after every addition in a canonical order (positions in order, bar lines first, then file order); the numerator stays within 32 bits of it because the duration cap (below 2^32 µs) is checked after every addition too. Exceeding the cap is a load error naming the measure, never a wrap. Display positions do not grow with the chart: only the scroll map's anchors touch the running time.
- **Cost.** Each position is one addition (two with a STOP), done with the reduced-sum method (Knuth, TAOCP vol. 2, 4.5.1), whose GCDs run against the term's small denominator only. At the cap an addition is about 8,000 limb operations, and the object limits allow about a million additions: an estimate of several seconds. A display position costs a few products of small integers per position. A scroll-map anchor costs, per BPM change and STOP end at a fractional µs, one division by a small multiple of the running time's denominator. Measured on the development PC (release build), a fixture driven to the cap loads in 4.0 s, against 0.9 s for as many events with a small denominator; a fixture that reaches the cap and then anchors 240,000 BPM changes, each with a kept note, loads in 3.6 s, 1.8 s of it for the anchors. A chart may use every hex BPM (least common multiple 362 bits) and about 800 further pairwise-coprime BPMs; not every chart within the limits fits this budget, since the 1295 `#BPMxx` definitions alone can reach about 130,000 bits.
- Big integers allocate on every operation, which is why time and positions are computed only while loading; the chart model holds only `i64` µs and `i128` positions.
- Alternatives: `Ratio<i128>` overflows on charts with many distinct BPMs, which then cannot load. Fixed-point time in sub-µs units, or integer µs per segment, rounds every segment, so objects drift onto other µs. `f64` depends on operation order and platform, and its 53 bits are too few for positions. Positions in millionths of a beat (`i64`) collapse low BPMs and saturate high ones; 64 or 72 fractional bits drift by about 220 or 0.9 pixels at the finest scale; exact rational slopes overflow `i128`; a big integer per object would allocate during play; and segments starting at the floored change time would anchor on the previous curve.

### `#RANDOM`

Branches follow the rule charts are written and tested against, with jbms-parser's `BMSDecoder` as the reference: `#RANDOM` and `#SETRANDOM` push a value wherever they are, and `#ENDRANDOM` pops one; `#IF k` pushes a flag, and `#ENDIF` pops whatever flag is on top, so `#IF`s left open stack and each block is read by its own value. `#SETRANDOM` and `#ELSE` are our additions; `#ELSEIF` is unsupported. Parity covers well-formed operands only: an operand that is not a positive 32-bit integer decides which notes exist, so it is a load error rather than whatever the reference happens to do. Draws use our own SplitMix64 with the seed recorded; `rand`'s output streams may change across versions, which would break replays.

### What the Loader Reads

The loader reads `#PLAYER 1` 7-key + scratch charts. Notation it cannot honor is refused, because the chart would play differently from how it was written and nothing tells the player: 2P lane objects, `#LNTYPE 2` and above, an `#LNMODE` other than LN, negative STOPs, and `#SWITCH`-family branching. Chart features beyond what the current stage shows wait for the stage that shows them. Until then the loader ignores without a warning the ones that change only what is shown: invisible notes (channels 31–39, which are neither drawn nor judged but change the key sound a press between notes plays), BGA, and the headers other players read for display (`#BASEBPM`, `#CHARFILE`, `#EXTCHR`, `#MAKER`, `#BMPxx`, `#BGAxx`, `#TEXTxx`). The others warn, since the chart then plays differently from how it was written: judge-rank changes (channel A0 with `#EXRANKxx`; the chart keeps its `#RANK`/`#DEFEXRANK`), background MIDI music (`#MIDIFILE`), scroll and speed changes, and unused channels. A `#DEFEXRANK` below 1 is ignored with a warning, as the reference decoder does, so an earlier `#RANK` stays. An invalid `#TOTAL` or `#DEFEXRANK` does not change timing, so it is ignored with a warning like any unreadable header value.

### Loading Limits

Charts, sounds and records are untrusted input. Every limit is checked before the work it bounds, and exceeding one is a load error with the reason. The defaults sit far above real charts; they bound work and are never tuned by playing.

- **Chart text**: at most 16 MiB, 500,000 lines and 500,000 objects; measures up to 999; the decimal digit and exponent bounds, the time's denominator cap and the display position range ("Chart Time"). `#RANDOM` and `#IF` depth is not limited: each line costs O(1) whatever the stacks hold, and the line count bounds their memory.
- **Duration**: at most 60 minutes, checked on the resolved chart before any capacity is sized from it, up to the last note, mine or BGM object. Bar lines, BPM changes and STOPs after that object are dropped, so empty trailing measures neither lengthen a chart nor draw bar lines.
- **Sound paths**: a `#WAV` name that is absolute, rooted, drive-prefixed, UNC, contains a colon (a drive or an alternate data stream), or has a `..` path component is rejected before joining (`a..b.wav` is an ordinary name). After the extension fallback (`.wav .ogg .flac .mp3`) the candidate is canonicalized and must still lie under the canonicalized chart folder, which catches links and junctions. An escape is a load error, not a skipped sound.
- **Sounds**: the decoding caps in "Audio".
- **Records**: the decoding limits in "Input Log Format".

## Input

- **Receiving**: A dedicated thread receives Raw Input (keyboards, HID dedicated controllers). MIDI is received in a separate callback and normalized to the same format. How times are attached follows the "Time Base (QPC)" section. Even when GameInput is also used, the same device is never ingested twice through two paths.
- **Keyboard**: Treated as the standard device that people without a dedicated controller use first. Input is recorded by physical key ID, so a standard key layout, scratch assignment, and a screen for checking simultaneous presses (to notice the keyboard's simultaneous-press limits) can be added later within the input layer.
- **Time**: A QPC time is attached at the moment the event is received (the receive time). It is the true receive time: the floor of the QPC read taken when the batch was received, the same for every event of that batch. The constant delay until the OS delivers it is corrected by the input latency offset.
- **Physical input is the source of truth**: What is recorded is "which button or axis of which device changed, when, and how". The mapping to lanes (key config) is recorded, and the evaluator applies it. This also reproduces multiple keys assigned to one lane and per-device debounce.
- **Debounce**: Not applied at the recording step. The evaluator applies it later as a device setting. It is not built yet: keyboards do not bounce and controller firmware usually debounces, so the device profile gains it, with a log version bump, when a device needs it.
- **Sources**: One physical device is one source. It owns one queue, one input latency offset, and all its controls, buttons and axes alike; several keyboards are several sources, even when they share a key binding. Sources get anonymous session-local ids (0, 1, … when recording opens: controllers first, then keyboards, each in the order found, so a keyboard's many Raw Input handles never push the controller out at the source limit); interface paths, names and serials never enter the input log. Source bookkeeping is preallocated with a fixed maximum, and a device beyond it does not take part; the input log is then marked "incomplete", and the devices that found no source are counted and named on the console before the countdown. Windows exposes each top-level HID collection of a controller as a device of its own; the collections of one function of a physical controller are grouped into one source by the device's container id and the function's interface number, so a source never holds two functions of a composite device, and each control carries its collection number, so the controls stay distinct. A collection whose path or container cannot be read, that belongs to no physical device, or that shares its collection number with another collection of its function (so its controls could not be told apart) is a source of its own; one whose path cannot be read has no identity, so key config cannot bind it. Removing any collection of a grouped controller removes its source, the function (container and interface number) it belongs to: the source is disconnected, releasing everything it held, and the state kept for all the function's collections is dropped. Another function of the same device stays connected. It comes back only through fresh discovery of its collections, as a new device with empty state, like a first connection. A controller keeps the metadata of at most 16 axes over all its collections; an axis beyond them is not read, so it can be neither bound nor recorded.
- **Participants are frozen**: The set of participating sources is fixed when play recording opens and does not grow during the play. A device that arrives during play is lifecycle metadata only: its arrival time and kind go into a bounded arrivals list outside the input stream and the watermark, and it takes part from the next play. As an input event, its arrival could carry an evaluation time below the already finalized watermark. The axes are frozen the same way: an axis is recorded only if its source declared it when recording opened, so one that a collection discovered during the play adds to an existing source is not recorded until the next play. A replayed log declares the axes its record binds.
- **What is recorded**: Only controls bound in the key config (lanes and system controls), and only while play recording is open. Unbound controls cannot affect evaluation, so an input log never holds typed text. While recording is closed, the key config screen may capture any control to bind it; nothing captured there is recorded.
- **Held-input snapshot**: The input thread keeps each device's physical state (pressed controls, and the last position of each absolute axis, 0 included) for the whole run of the game, across retries. A control of a device not seen since the game started counts as released. Recording opens at a boundary between two batches: the held state of the participating sources is copied into the start snapshot of the play, and every event pushed after that point, in stream order, belongs to the play. Input conversion starts a held control as pressed with no press edge, so nothing is judged from it, and its later release is a plain release. A release of a control that is not held changes nothing.
- **Removal**: Removing a participating device records a disconnect event for its source (see "Analog Scratch" for what input conversion does with it). The source publishes the removal time as its final watermark and is closed. It stays in the watermark minimum until all its queued and staged events are merged, and is retired only then, so a removed device neither stalls the watermark nor loses an event. Removing the last participating device aborts the play.
- **Roles of controls**: A bound control has exactly one role: a lane (judged), a system control (retry, quit), or a play adjustment (hi-speed, volume, display offset; never judged). One control bound to two roles is refused. For now the game reads system controls and adjustments from window keyboard messages, so only keyboard keys can serve them, and key config accepts only a keyboard key for retry and quit. Later the input thread classifies every control from the Raw Input stream, so controller buttons can serve them too, and modifier combinations become possible: while a modifier such as a start button is held, lane presses act as adjustments and are excluded from judgment. The modifier is a recorded control and the rule is recorded with the bindings, so re-evaluation gives the same result; it arrives with a log version bump.
- **Focus loss**: Losing focus during play aborts the play. The window's focus-loss notification sets a latch that the input thread checks before each batch, and any Raw Input packet received while the game was in the background is rejected before it can be recorded and trips the latch too. Once the latch is set, nothing more is appended; the play ends at the last finalized watermark and is saved as "ended midway". The notifications are `WM_ACTIVATEAPP` with `FALSE` and `WM_KILLFOCUS`; a background packet is one whose header carries `RIM_INPUTSINK`, which also catches focus changing between the latch check and the batch. If aborting on every focus loss gets in the way (a notification stealing focus), the rebuild is pausing with a recorded gap, which needs an operations stream and a log version bump, not a change to the watermark rule.
- **Raw Input reception**: The input thread owns a message-only window registered with `RIDEV_INPUTSINK | RIDEV_DEVNOTIFY`. It follows the buffered pattern with device notification: a `WM_INPUT` taken off the queue is read with `GetRawInputData`, the rest is drained with `GetRawInputBuffer`, and `WM_INPUT_DEVICE_CHANGE` is handled in the same loop, with no message filter that would drop it. Raw Input has one target window per device type per process, so `player` calls winit's `listen_device_events(DeviceEvents::Never)`. HID reports are read up to 4 KiB; a larger one, or a read that fails, is lost input and marks the log incomplete. Packets that cannot be attributed to a participating device are not recorded. A controller that reports in a vendor format instead of standard HID usages needs its own input adapter.

### Analog Scratch

Axis values are recorded raw with timestamps, and the evaluator converts them into scratch presses and releases based on the device settings. The settings include the following, and all of them are recorded in the replay.

- Whether the value is absolute or relative, and wraparound on a full revolution. Both rotation directions press alike, so there is no inversion setting; one arrives with a rule that tells the directions apart
- The rotation threshold for counting as a press, and the time after movement stops before counting as a release
- The rule for combining button input and analog input on the same lane
- When a device is disconnected, a disconnect event is recorded in the physical input stream. On it, input conversion releases every input of that device that was pressed and clears that device's pending debounce and scratch state, so live play and re-evaluation handle it identically

The conversion, with movement counted in the axis's units after wraparound (a change of more than half a revolution is taken the short way round):

- Released, the axis presses its lane once it has moved the press threshold in one direction. The count starts again when the direction changes or the axis pauses longer than the release time, so a position flickering at rest never presses.
- Pressed, movement of the hold threshold in the pressed direction keeps it pressed (the hysteresis: less movement holds than presses), and movement of the press threshold the other way is a new press. Every press and hold sets the release deadline the release time later; the evaluator releases the lane when the watermark passes it. At equal times inputs still come first, so a report at the deadline moves it; among deadlines a scratch release comes first, as the release input it stands for would.
- A button and an axis on one lane combine like two buttons: the lane is held while either holds it. An absolute axis whose position is unknown (its first report, when the held-input snapshot does not list it) only learns where it is. The axis is never pressed at the start of a play: the snapshot holds positions, not presses.
- The range is the device's: whether the axis is relative, and for an absolute axis its declared logical range, any signedness and any field width, whose maximum − minimum + 1 values are one revolution (a field unsigned to its full width declares its maximum as −1, which is read back unsigned). At play time only the declared range or the `device.toml` override applies; a declared −127..127 is 255 positions, as a genuine 255-position axis needs. Where a descriptor understates the range, key config corrects it from what it sees: once `tapestry keys` has captured an axis for the scratch, the player turns it a few full revolutions while key config watches its reports, until it has crossed its boundary both ways, 10 s have passed, or F12 skips. If it reported values outside its declared range, the smallest standard span holding the declared and the observed values (the full two's-complement range of k bits for a signed field, 0..2^k − 1 for an unsigned one, 2^k values, k never beyond the field's width) is written to `device.toml` as the explicit revolution override; the screen shows the declared range, the observed range and the revolution used.
- The scratch has two directions. An axis covers both. A button or a key covers one, so key config then asks for the other direction (F12: none), and both controls play lane 0, combined as any two controls of one lane. Nothing assumes a particular controller's range. `device.toml` can set the mode and the values in a revolution instead; the log records the resolved settings. The thresholds and the release time default to press 2, hold 1 and release 100 ms after the last press or hold, starting points tuned by playing. An axis whose range is unknown (its device not connected) does not wrap.
- Descriptors can be wrong. A value reported outside the range the conversion uses is recorded as reported and converted with the recorded settings, so re-evaluation gives the same result as live play; the input thread counts such values per source, the player prints the count once at the end of the play, and the log records it with the source, so a record whose conversion may be off says so. That range is the declared one unless `device.toml` sets the revolution, and then the standard span of that many values (0..n − 1, or from −⌊n/2⌋ to −⌊n/2⌋ + n − 1 for a signed field, so −128..127 for 256 and −127..127 for 255, as key config writes it; the signedness is the field's, kept for an axis declared relative); the printed count names which of the two it was checked against. A relative axis, or one that does not wrap, has no range and counts nothing. Setting the range in `device.toml` corrects later plays.
- **Report rate.** An absolute axis is recorded at its first value and then at each changed value, and a relative axis at each nonzero delta. So an axis costs one event per report that changes, jitter at rest included. Neither buttons nor keys cost more with a higher polling rate: HID buttons are recorded at state changes, and keyboard packets as they arrive, auto-repeat included. A device that polls faster than 1 kHz is recorded as it reports: its reports within one input-thread wake stay separate events with the same receive time and are never merged, because a movement out and back within one wake can be a press and a reversal. The play's input capacity counts each declared axis at 1000 events a second, so such a device can exhaust it within a play of ordinary length ("Recording Capacity").
- HID controls are named by usage: a button by its number (page 9), an axis as `page << 16 | usage` (X is `0x1_0030`). Keyboards share one key config by scan code. Controller bindings belong to one controller: `tapestry keys` saves the model (vendor and product id) of the controller whose controls it bound and, on a composite device, the interface number of the function that holds them, and refuses a control of another controller or function during the same key config, a control of a controller it cannot identify, and keeping current controller bindings that name no controller. Only the first connected controller of that model and function takes part in a play; any other controller, another function of the same device or a second unit of the same model included, takes no part and is neither recorded nor counted in the recording window, so a gamepad's or a keyboard's joystick collection never plays a lane. The identity is the same in every port and after replugging, names no single unit, and never enters the input log. A `device.toml` whose controller binds a lane without its model is refused, with the hint to run `tapestry keys` again. When the configured model is not connected, the play uses the keyboard bindings only and says so on the console.

### Watermark

The watermark is the boundary that "no input with an earlier evaluation time will arrive from now on". The evaluation time is the receive time minus the input latency, converted to the chart's timeline.

- Each input device is its own source, with its own dedicated queue and its own input latency offset (devices sharing one receive path, such as several Raw Input devices, are still separate sources). A source puts all the events of a batch into its queue, then reads QPC afresh and publishes `w_i = floor(fresh QPC) − 1 µs`. This order is kept. Any later batch takes its own QPC read after that, so it is stamped at or above the fresh value and never at or below a published `w_i`; receive times stay true. Within one source, events keep their receive order, and because one offset applies to the whole source, that is also their evaluation-time order.
- The overall watermark is the minimum, over the participating sources not yet retired (see "Removal" under "Input"), after converting each source's receive time into an evaluation time: `min_i(w_i − offset_i(w_i))`. When every source is retired there is no watermark, which only happens when the play is aborted. The logic thread merges the events of the queues into one stream ordered by evaluation time, with a fixed tie-break (source index, then the order within the source), before passing them to the evaluator.
- For sources that deliver nothing on their own, such as a MIDI device with no input, the input thread periodically publishes on their behalf a notice that "there is nothing up to this time" (a receive-time update), so the watermark does not stall.
- Events with the same time as the watermark are guaranteed to have been published before the watermark is published.
- The logic thread's consumer, each step: (1) load every source's `w_i` with `Acquire`; (2) pop that source's events into its staging buffer, stopping at the first event received after `w_i`, which stays staged; (3) compute the overall watermark `W`; (4) merge the staged events with evaluation time ≤ `W` and push them to the evaluator; (5) `advance_to(W)`. Later events stay staged. Reading the watermark after popping would let a press pushed in between be skipped while its miss is finalized.
- The input thread wakes at least at 1 kHz and advances the watermark even when there are no new events.

> **Operations unrelated to judgment go in a separate stream**
>
> Hi-speed and lane cover changes are recorded as a stream separate from the judgment input. The evaluator reads only the judgment stream.

## Evaluator

The evaluator is a discrete-event processing system that puts input events and note deadlines (miss for an unhit note, LN end, HCN gauge increase and decrease, mines, song end) in a single queue and processes them in time order. It depends neither on frames nor on a fixed time step.

```
pub struct Evaluator<'a> { /* Judgment state. Does not own the chart itself; references it */ }

impl<'a> Evaluator<'a> {
    /// Input conversion starts from the start snapshot (held controls are pressed, with no press edge)
    pub fn new(chart: &'a Chart, rule: &'a RulePreset, device: &'a DeviceProfile, start: &InputStart, input_capacity: usize) -> Result<Self, EvalError>;
    /// Refuses (without change) an input beyond the fixed capacity, at or before the finalized watermark, or earlier than the last one
    pub fn push_input(&mut self, ev: PhysicalInputEvent) -> Result<(), PushError>;
    /// Finalizes events up to the watermark and appends judgment events to out
    pub fn advance_to(&mut self, watermark: Micros, out: &mut Vec<JudgeEvent>) -> Result<(), AdvanceError>; // refuses a backward watermark
}

/// Batch evaluation used for results, replays, and IR: merges the recorded sources as live play does,
/// pushes every input, and advances to the finalized watermark of an aborted play, or else to the end
pub fn evaluate(chart: &Chart, input: &EvalInput, rule: &RulePreset) -> Result<Evaluation, EvalError>;
// EvalInput: the recorded events, the device profile, the start snapshot, and the finalized watermark if aborted
```

Section evaluation ("Section Evaluation") adds its conditions to `evaluate` when practice is built (stage 2); until then it evaluates whole plays only.

### Event Order

- `Evaluator::new` builds, per lane, one array of chart deadlines from the chart and the preset: LN end deadlines, miss deadlines of taps and LN heads (note time plus the late BAD edge) and mine times, sorted by `(time, kind)` with the kinds in that order. An LN end comes before a miss at its µs, so the notes of a lane are judged in note order even when a scaled late window puts a later note's miss at the end of the long note before it. Sorted notes do not give sorted deadlines (a tap at 1000 ms misses at 1180 ms, after a mine at 1100 ms), so the arrays are sorted explicitly. A pending scratch release comes before every chart deadline at its µs, as the release input it stands for would, and one song-end deadline follows every other deadline at its time. Inputs come before deadlines at equal µs.
- Each deadline is skipped by its own condition when reached: a miss deadline if its note's head was taken (judged, or held); an LN end deadline if its head was taken and the long note is neither held nor visually held (an untaken head still waits for its press or its miss, and its end does nothing); a mine never.
- `Evaluator::new` refuses a chart with a note time or long-note end outside ±2^32 µs, the chart time domain, whatever built it, and a rank whose scale takes a window or LN-end edge beyond 2^32 µs. With both bounded, a press deviation and a release's distance from its long note's end fit in `i64`; comparisons with window edges are done in `i128`.
- Deadlines that input creates or moves during play, such as a pending scratch release, live in the conversion state and take part in the same scan.
- The scan compares the merged input head, the lane deadline heads and the pending conversion deadlines by `(time, kind, lane, seq)`. The live logic thread and `evaluate` call the same merge and the same constructor, so they cannot diverge. A `BinaryHeap` of deadlines would allocate as they are pushed, need the same explicit tie key, and pay `log n` for streams already in order.
- `push_input` appends to a ring sized at construction and `advance_to` writes into a caller-owned vector with reserved capacity, so neither allocates during play.

### Finalize Only Up to the Watermark

During play, only `advance_to(watermark)` is called. Even for a note whose deadline has passed, the miss is not finalized until the watermark passes that deadline. Therefore, a miss is structurally never decided ahead of an input that was still on its way. The finalization delay is determined by the watermark interval (about 1 ms with the input thread's 1 kHz wake-up). It cannot be guaranteed on Windows, so it is measured and tracked.

### What Wakes the Logic Thread

- It wakes both on input event publication and on watermark updates. Even without input, misses, HCN, mines, and song end are processed on watermark updates.
- Even if a wake notification is missed, advancing to the latest watermark on the next wake gives the same result.
- Input that does not fit is never silently dropped, and the input log is recorded as "incomplete" and excluded from replays and IR. A source's queue that is full drops the incoming event, and the play continues without it. Input beyond the capacity the play reserved before it started aborts the play at the last finalized watermark, and it is saved as "ended midway".

### The Only Output Is the Judgment Event Stream

- Judgments (note, judgment type, deviation in µs, and for a long note finished by a release its distance from the end), combo changes, gauge changes, key sounds to play, and LN state changes are emitted as timestamped events. Events carry sequence numbers.
- `Judged` is the only event that scores a note: each judgeable note gets exactly one, and only it changes the judgment counts, combo, EX score, BP and the gauges for that note (an empty POOR and a mine hit keep their own effects). A long note pressed before its end and graded GOOD or better emits `LnHeld` at the press, and every long note whose head a press took before its end (held, or decided BAD) emits `LnHoldEnded` once when that visual hold ends: at the release of its lane, at or before the end, or at its end deadline while still held, before the long note's judgment when both come at once. Both are feedback only and change nothing scored. A play aborted while a long note is held finalizes no judgment for it, so it is not counted.
- Key sounds, bomb effects, and score display are the job of whoever receives the events. The evaluator touches neither audio nor skins.
- Which note's sound plays on a press also varies by rule, so the evaluator emits the "sound to play" as an event.

### Determinism

- Time is integer µs, and the gauge is fixed-point (the rounding rules are fixed by the evaluator version). No floating point is used.
- The evaluator's purity is checked in part: `cargo xtask check-arch` (R1) bounds its dependencies, and `crates/eval/clippy.toml` rejects the named `std` entry points for the clock, the file system, the environment, threads and printing, and `std`'s randomly ordered hash collections. Anything outside those lists stays a reviewed invariant. In `eval`, `rules` and `bms-parser`, clippy rejects `as` casts that may truncate, wrap, or lose the sign.
- The processing order of events at the same time is made explicit as a preset item.
- Anything that uses randomness (`#RANDOM` branches, RANDOM-type options) is decided at the step that resolves the chart before evaluation, and the result is recorded. The evaluator itself uses no randomness.
- Even for the same chart file, a change in the loader version can change its interpretation. So the loader version and the hash of the resolved chart are recorded, and re-evaluating a saved record against a freshly loaded chart (`eval-cli`, local input logs) refuses a record whose loader version or resolved-chart hash differs from that chart's, as it refuses another evaluator version.

## Rule Presets

Rule differences are expressed in three ways: data (numbers in TOML), fixed choices (`enum`), and rule-specific implementations built into the evaluator. All of them are code inside the evaluator, not plugins that users load later. A preset holds the data and choices plus the name and version of the evaluation implementation to use, and can be serialized in full and hashed. This is so that which rule was used for evaluation is reliably recorded. Before the first release, the preset file and its canonical encoding may change incompatibly: an older one is refused, not converted, as with replays.

| Level | Contents |
| --- | --- |
| Data | Judgment windows (per judgment, early and late sides separately; separate tables for normal notes and scratch), and the LN-end table holding the sides its LN mode reads (LN: the early edges of PERFECT, GREAT and GOOD)<br>Conversion table from `#RANK` / `#DEFEXRANK` to judgment window multipliers, default value when `#TOTAL` is absent<br>Empty POOR range<br>Gauge (initial value, lower limit, clear threshold, per-judgment increase and decrease, reduction at low remaining gauge)<br>Score weights (EX score), what counts toward BP, processing order of events at the same time |
| Choices | How a press is assigned: nearest note / oldest unprocessed note / combo priority<br>How `#TOTAL` applies: multiplies the increase / also corrects damage<br>Mine condition: held while it passes / pressed at its time |
| Fixed | Judge by input time, process in time order, survival gauges never recover once they reach 0, time is integer µs |

> **LN "notation" and "judgment mode" are different things**
>
> `#LNTYPE` and `#LNOBJ` express how LNs are written in the chart. The loader interprets them and reconstructs the LN spans (start and end). Whether to judge them as LN, CN, or HCN is decided separately: it is a play option the player chooses, not a preset item, so one preset serves all three modes. If the chart specifies a judgment mode (such as beatoraja's `#LNMODE`), the chart's mode is used. The mode actually used is recorded in the input log, and best scores and clear lamps are kept per chart, preset and mode in effect.

### LN Judgment Modes

LN, CN, and HCN are one long-note rule with different settings, not three separate implementations. The preset holds the windows and gauge amounts each setting reads.

| Item | LN | CN | HCN |
| --- | --- | --- | --- |
| End window | Early edges only: a release before the end is graded by the LN-end edges (PERFECT, GREAT, GOOD; earlier than every edge is BAD); a release at or after the end, or holding through the end, leaves the head's judgment | Graded on both sides, with a POOR beyond the late side | As CN |
| Judgments per LN | One: the worse of the head's and the release's | Head and end each judged | As CN |
| Gauge while held | None | None | At a fixed interval, increases while held and decreases while released; pressing again during the body resumes the increase |

In LN mode a long note is judged once. Its head is graded by the note windows. A head graded BAD decides the long note at the press (a later release changes nothing), and a head pressed at or after the end decides it at the press with the head's judgment. A head graded GOOD or better before the end is held: the long note is judged at the release that ends the hold, as the worse of the head's judgment and the release's by the LN-end edges (inclusive, like a note window), or at its end with the head's judgment when held through it. A missed head is one POOR. There is no separate LN-break penalty: the one judgment carries combo, BP, gauge and EX score by the usual per-judgment rules. The LN-end edges scale with `#RANK`/`#DEFEXRANK` by the factor of the note windows (each edge scaled in µs and floored). Preset validation requires the LN-end edges to strictly ascend from PERFECT to GOOD, PERFECT not negative, and each to be at least the note windows' early edge of the same judgment: a press at deviation `h` held at least as long as its long note (length `L`) releases at `h + hold − L ≥ h` from the end, which the LN-end edges then never grade below the head, so holding at least as long as the note keeps the head's judgment on every chart.

For now only LN is judged, and a chart whose `#LNMODE` asks for CN or HCN is refused. Its preset still selects the mode (`ln_mode.default`, with the LN variant alone); that selection leaves the preset when CN and HCN arrive, together with the log version that records the mode.

### Standard Presets

- **Custom (default)**: Judgment specific to this player. Its values differ from the judgment of the commercial game this is based on.
- **beatoraja-style**, **LR2-style**: Aim for similar behavior based on public information. They are named "-style", not "compatible", and an exact match is not promised.

Items are added when a preset that needs them appears. Unused tuning items are not built in advance.

### Custom Preset

The custom preset (`rules::custom()`, written to `presets\custom.toml` on first start) is the only preset so far. Its values and behaviors are starting points chosen by what the player experiences and tuned by playing; another player's numbers are not copied, since they would carry that implementation's choices without anyone having felt them.

| Item | Value | What the player experiences |
| --- | --- | --- |
| Judgment windows, early = late, one table for every lane | PERFECT ±20 ms, GREAT ±45 ms, GOOD ±100 ms, BAD ±180 ms | A press that feels on the beat is PERFECT. Clearly off but intended reads as GOOD, and only a wrong press is BAD |
| Miss | No press by the late BAD edge → POOR. A press that judges a note also passes every earlier unjudged note of its lane: each is a POOR at the press, in note order, before the press's own judgment, with the usual miss effects (a passed LN head loses its LN) | A missed note is decided as soon as it cannot be hit any more, and notes are never judged out of order |
| Empty POOR | A press that judges no note but has a note of its lane, judged or not, within 500 ms after it or 150 ms before it (both edges inclusive); mines are not notes here. Every such press counts, with no cap. No combo break, BP +1, not counted in judgment counts; gauge damage half of a POOR | Mashing early, or again right after a note, is noticed and costs a little, but does not end a combo |
| Combo | Broken by BAD and POOR | — |
| Note assignment | Nearest unjudged note inside BAD, ties to the earlier one | An early press does not steal the next note |
| LN mode | LN: one judgment per long note, the worse of its head's and its release's ("LN Judgment Modes") | Holding to the end matters, but the release need not be exact |
| LN-end windows | PERFECT 100 ms, GREAT 150 ms, GOOD 200 ms before the end; earlier is BAD. A release at or after the end keeps the head's judgment | Letting go a little early costs nothing, a clearly early release costs one grade at a time, and only letting go long before the end turns the note into a BAD. A long note is never worth less than a tap pressed with the same timing and held for its length |
| Late head after a short LN's end | A head pressed within its late window but at or after the LN's end decides the long note at the press with its own judgment; the following release is ignored | Very short LNs behave like taps |
| Mines | Damage every gauge by the chart's value when the lane is held as the mine passes, 0.5 percentage points per unit (`1E` = 50 units = 25 %), not reduced by the survival gauge's low-gauge halving. No judgment, combo or BP effect | — |
| Gauge denominator | Judgeable notes = taps + long notes, one judgment each; mines excluded | — |
| `#RANK` scale | 0: 50 %, 1: 75 %, 2: 100 %, 3: 125 %; `#DEFEXRANK n`: n %; no rank, or a `#RANK` outside 0–3: 100 %. Each window edge and LN-end edge is scaled in µs and floored; an edge beyond 2^32 µs, before or after a scale, is refused | Harder ranks shrink every window by one factor |
| `#TOTAL` absent | 160; a `#TOTAL` that is not positive counts as absent | An all-PERFECT play fills the normal gauge from 20 % to full at about half the chart |
| Normal gauge (decided at the end) | Start 20 %, clear 80 %, PERFECT and GREAT +TOTAL/notes, GOOD half of that, BAD −2 %, POOR −6 %, floor 2 % | — |
| Survival gauge | Start 100 %, PERFECT and GREAT +0.16 %, GOOD 0, BAD −5 %, POOR −9 %; damage is halved when the value before the change is below 30 % (exactly 30 % takes full damage; the halved change is floored with its sign) | — |
| EX score | PERFECT 2, GREAT 1 | — |
| Zero judgeable notes | The evaluator refuses the chart (`NoJudgeableNotes`), and the session does not start | Nothing to play, rather than a division by zero |

Each choice enum (`Assignment`, `LnMode`, `TotalMode`, `MineRule`, `SameTimeOrder`) has only the variants a preset uses; the variants later presets need arrive with them, and a rule no preset uses any more is removed with the code that evaluates it.

### Handling Differences Not Expressible as Numbers

What is kept is not "express everything with data only", but the contract as seen from outside the evaluator: the process that produces the judgment event stream from `(chart, input log, preset)` is pure and deterministic, and is recorded together with versions. Within this contract, support is extended in the following order.

1. **Differences expressible as numbers**: Handled by preset data.
2. **Differences not expressible as numbers**: Add one choice to the relevant axis (e.g. the condition for missing an unhit note, whether an empty POOR breaks the combo). Rule differences are gathered into one module per axis, so that adding a choice stays within that axis. Do not scatter code that branches on the "whole rule" across the judgment code.
3. **Too different to split into axes**: Add one evaluation implementation dedicated to that rule inside the evaluator. Presets select it by name and version, and the contract seen from outside does not change.

There is no need to exhaust this order. If several behaviors are intertwined and splitting them into independent axes would be unnatural, a dedicated implementation from the start is fine. Only combinations of axis choices whose behavior has actually been verified are allowed. Start with one evaluation implementation and concrete examples, and add axes or implementations only when an actual rule demands them.

Differences are found in the process of writing hand-written examples per preset ("this input gives this judgment"), and each time one is found, an axis or a choice is added. For a change that alters meaning, the evaluator version is bumped and the results are not mixed with past records. A preset naming an evaluator version this build does not run is refused by preset validation and `Evaluator::new`, and re-evaluation refuses a record made by another evaluator version before it compares preset hashes.

## Gauges and Session Policy

Gauges are divided into two kinds by how a clear is decided.

| Kind | Examples | Clear condition | Failure |
| --- | --- | --- | --- |
| Decided by the amount remaining at the end | ASSIST EASY, EASY, NORMAL | The amount remaining at song end is at or above the clear threshold | Never fails midway |
| Decided by survival | HARD, EX HARD, dan-course gauges, etc. | Stays above 0 through the end of the song | Finalized the moment it reaches 0 and never recovers afterward (recorded in `failed_at`) |

- All gauge types included in the preset are computed side by side.
- For survival gauges, separately from the real value (which stops at 0 on failure), "the trajectory had it not failed" is computed in separate state as a reference value. The reference value is not used for clear determination.
- Gauge history is recorded for every judgment event. It is not thinned out at fixed intervals.
- Values are `i64` in units of 1/100000 % (100 % = 10,000,000). With `#TOTAL` in thousandths, the per-note increase is `floor(total_milli × 100 / judgeable notes)`. An `i32` in 1/100 % would round the per-note increase of long charts to 0.

### Session Policy Lives Outside the Evaluator

The following three are not rules but policies for how play proceeds, and they are kept outside the evaluator. The evaluator only computes all the gauges; it does not decide which one to display or when to stop.

- **Choosing the displayed gauge**: fixed / automatically switch to the highest surviving gauge (GAS)
- **Handling failure**: stop there / keep playing to the end
- **Automatic offset adjustment**: disabled / enabled (adjustment rules). See the "Time and Offsets" section

> **What recomputed results mean**
>
> A result recomputed under another rule is the value of "what if the same input were judged under that rule", not "what would have happened had you been playing under that rule". Players change how they press based on the judgment display on screen, and once play stops on failure there is no input after that. The screen shows this difference in its wording, and the part after the stop is shown as "no input". With the policy of playing to the end, the input log is always complete through the end.

### Play Lifecycle

A play goes through countdown, play, evaluation complete, audio tail, save and result. Retry and quit work from any state.

- **Lead-in**: chart time 0 is 1 s after recording opens, which starts the countdown (tuned by playing), so the first note cannot arrive before the player is ready. The play's setup (planning, the reservation and building the logic step, bounded by the chart and the reservation) runs at the countdown's start, before its first frame, and is measured ("Latency Figures"). A retry reuses the decoded audio and GPU resources and only resets the session.
- **Completion**: the final deadline is the latest applicable deadline of the chart (misses of taps and LN heads, LN ends, mines). Evaluation is complete when the global watermark reaches it, not when QPC time does; with a positive input offset these differ. The evaluator then emits `SongEnd`, recording closes, and its finalized watermark is frozen, so the recording window and its capacity end at finalization.
- **Abort**: retry, quit, focus loss, removal of the last participating device, suspend, input beyond the recording capacity, an audio output failure, a screen failure, an input thread that ends, and a failed survival gauge when the session policy ends the play there. A command, input-thread failure, focus loss, or audio or screen failure already seen in a step is handled before that step advances (an input-thread failure before focus loss, whose flag the input thread publishes), so a retry in the step that would emit `SongEnd` ends the play midway. An audio failure noticed after the advance, and a survival gauge the advance failed, are checked after it: if that advance completed the play, completion wins and the play stays complete. The play ends at the last watermark the evaluator had already finalized; staged and later events stay in the record while capacity remains but are not evaluated. The record stores that watermark with the end status "ended midway", and re-evaluation advances to it instead of to the end. A suspend while recording is open sets "incomplete" (reason "suspend"), because the clock jumped, whichever cause ends the play; an overflow sets it with the reason "overflow". The other causes do not set it themselves, and a flag already set by a queue overflow or the source limit stays. A play aborted before its first advance has no finalized watermark and saves no record.
- **Input thread failure**: opening and closing recording wait for the input thread at most 2 s of awake time (a suspend during the wait does not count), far beyond the wake at which a live thread answers; quit is latched and taken after the wait. A thread that is gone, or does not answer, fails the link for good: no later request is sent, so a late reply is never read as another's. A failed open ends the run with that cause before any play. A failed close keeps what the play already took and loses what reached the queues after its last step, so the record is saved as usual, with no arrivals or closing flags, and marked "incomplete" (reason "input failed"); a completed play stays complete and loses its tail. An input thread that ends during play aborts it like any other cause, and one that ends after a close that succeeded cuts the tail only. In every case the run ends with the input cause at the next quit or retry, after the report and the end screen; on exit the input thread is told to stop but not joined, so one that is stuck cannot hold up the exit.
- **Audio tail**: after `SongEnd` the play screen stays until the audio engine has no active voice and no BGM event left. This is session policy, not evaluation. Nothing is recorded; retry, quit, an input-thread failure, focus loss, removal of the last device, an audio failure or a screen failure cuts the tail short, and the record stays complete.

## Input Logs and Replays

A replay holds the inputs needed to reproduce evaluation. Judgment results are kept only as a cache and are not treated as the correct values. The format carries a version number, and fields can be added by bumping the version. Before the first release, formats may change: a change bumps the version, and records of older versions are refused, not converted.

| Field | Contents |
| --- | --- |
| Versions | Replay format version, evaluator version, loader version, app version |
| Chart | SHA-256 and MD5 of the chart file (MD5 for compatibility with existing difficulty tables and IRs), hash of the resolved chart |
| Chart resolution | `#RANDOM` branch results, the chart transform applied (an id and the version of its rules, such as the pass-through or RANDOM and MIRROR options) and its seed, the resolved lane mapping |
| Rule | Preset ID, content hash, a copy of the preset itself. Session policy |
| Device | Device type, receive method (Raw Input, MIDI, etc.), the record of report intervals and the estimated polling rate (or "not estimable"), key config, debounce, analog scratch settings, the three offsets |
| Play type and evaluation conditions | Play type (full, mid-chart start, section repeat), section, attempt number, lead-in length, speed (the mapping between real time and chart time), initial gauge value (see the "Practice Records" subsection), the LN judgment mode in effect (added with the log version that brings CN and HCN) |
| Physical input | A sequence of `(pre-evaluation receive time µs, source ID, button/axis ID, value)` with the disconnect event of a source in the same stream. The held-input snapshot (`InputStart`: the time recording opened and the controls held at that point) precedes it. Devices that arrive during play are a separate arrivals list (time and kind), outside the input stream. Times are delta-encoded per source and compressed |
| Offset changes | Changes applied by automatic offset adjustment (time, new value) |
| Other operations | Hi-speed and lane cover changes (a stream separate from the judgment input) |
| Status | End status (played to the end, ended midway), the finalized watermark at the end (an aborted play is re-evaluated up to it), whether input is incomplete and why. Start token for IR (optional) |

The format uses a custom schema, and version detection does not rely on the hash of the serialized output. For a chart of about 2000 notes, it fits in a few tens of KB even before compression.

### Input Log Format

- **Layout**: the magic `TPRL`, a `u16` format version, a `u8` body encoding (1 = zstd), a `u32` uncompressed body length, then the compressed body: a fixed sequence of sections, each a `u32` length and its payload. Integers are LEB128 varints (zigzag for signed values), and input times are deltas from the previous event of the same source. Records are saved as `records\<chart sha256>\<n>.tprl` under the data root.
- **Recorded today**: the versions with the build provenance (`production | development`); the chart identity, `#RANDOM` results and the chart transform's id and version; the preset record; the session policy id; the device profile per source (kind, receive method, input offset, bindings, axis bindings with their resolved scratch settings, and the count of axis values outside the range their conversion used) with the audio and display offsets; the play type (always full); the start snapshot, the physical input with disconnects, and the arrivals list; and the status with the finalized watermark and the reasons for "incomplete" (overflow, source limit, suspend, input failed). Report intervals and polling estimates, offset changes, the other-operations stream, the practice fields and the IR start token arrive with the features that produce them, each with a version bump.
- **Preset identity**: the hash of the preset's canonical binary encoding, not of its TOML text, so whitespace and comments do not split records. The encoding starts with its own version; a record made under an older layout matches no preset, and its re-evaluation is refused as a preset mismatch.
- **Versions**: each layout change bumps the format version, and the decoder reads only the current one; older versions are refused with `UnsupportedVersion(n)`, not converted. Reading older versions starts with the first release.
- **Decoding limits** (the standard ones): at most 64 MiB compressed; a declared uncompressed size of at most 64 MiB, checked before decompressing, with decompression stopping at it; section lengths against the remaining bytes; at most 64 sources and 4,194,304 events; and bindings and axes against the evaluator's limits (at most 256 buttons and keys and 8 axes per source, 1024 and 16 over all sources), counted as each count is read and before anything is allocated for it. The evaluator refuses a profile beyond them too, and finds an input's axis by binary search over its source's axes, so a log that declares many controls cannot make conversion slow.
- Alternatives: `serde` with `bincode` or `postcard`, whose wire layout follows the Rust struct layout and changes silently with a field reorder; JSON, about 5–10× larger; protobuf, a schema compiler for a format with one reader and one writer.

### Practice Speed Changes

- A speed change is expressed as a mapping between real time and chart time (a constant multiplier per segment). The chart itself is not modified.
- Judgment windows are measured in real time. Even at 0.5x speed, the tolerance from press to judgment is the same as normal.
- Audio is played back matched to the speed, and the player chooses whether to preserve pitch.

### Section Evaluation

Mid-chart starts, section repeat, and excerpts from full-play records are all handled by the same "section evaluation" rules. A section is `[start, end)` in chart time.

- **Target notes**: Only notes whose time falls within the section are targeted. An LN that crosses the start or the end is excluded entirely.
- **Input acceptance range**: Decided separately from the selection of target notes. Presses during the lead-in before the start are accepted only if they fall within the early judgment window of the first target note. After the end, too, the watermark is advanced and judgments are finalized up to the last judgment deadline of the target notes (late judgment window, miss deadline, LN release deadline).
- **Evaluator state**: Judgment and gauge state are initialized at the section start. The initial gauge value (a fixed-point value) is set by the practice settings.
- **Input conversion state**: The keys pressed at the start, pending debounce, and the accumulated scratch and hysteresis state are reconstructed from all physical input before the start: from the play's held-input snapshot and every event up to the start, not only the lead-in. A key held down across the start is treated as already pressed at the start and is not counted as a new press. Judgment and gauge state are reset separately, as above.
- **Offsets**: The recorded changes are used as is, including the value at the start and any interpolation segment in progress.

`evaluate` will accept these section conditions (section, lead-in length, initial gauge value, offset changes); without them, it evaluates the whole chart. The start snapshot and its conversion setup, which section evaluation reuses, exist already.

### Mid-Chart Start and Section Repeat

- **Mid-chart start**: As a practice feature, play can start from any position in the chart. Judgment follows the rules above and starts as a new evaluation session at the start position.
- **Restoring audio and screen**: Up to the start position, autoplay with the same speed and settings fast-forwards to restore long sounds still playing, the BGA, and effect state. Key sounds and bombs are determined by judgment events, so what to restore them from has to be decided. Normal practice restores from autoplay; replay playback restores from the recorded input. Audio processing advances as usual rather than feeding silence, and only the final output is discarded.
- **Section repeat**: Uses mid-chart start repeatedly. Each round proceeds in the order reset → restore → lead-in → section evaluation, and each round is delimited as one evaluation session.
- Tests verify that the restored state matches (outside judgment) the state at the same time reached by advancing from the beginning with the same source (autoplay or recorded input).

### Practice Records

- Records with a changed speed and practice records also keep their input logs instead of discarding them, so they can be used for analysis.
- As evaluation conditions, a record stores the play type (full, mid-chart start, section repeat), section, attempt number, lead-in length, speed (the mapping between real time and chart time), assist options, and initial gauge value. Whether play reached the end or ended midway is a separate field, the end status.
- Only records evaluated under the same section evaluation rules can be compared with each other. From a full-play record, any section can be excerpted and re-evaluated under the same rules, so it can be compared with a mid-chart start. A full result cannot be produced from a mid-chart start record, so comparison in that direction is not done.
- When evaluating an excerpted section, the offset changes from the original record are used. Whether those changes follow the automatic adjustment rules can be verified only on the original full record.
- For records at different speeds, metrics measured in real time (such as the deviation distribution) are shown side by side per speed. A different speed changes the difficulty, so they are not treated as the same result.

### Autoplay

`tapestry play --auto <chart.bms>` plays the chart by itself in the production build, on the real window, audio output and renderer, to watch a chart and hear it as charted. Nothing it plays is saved. One choice made while the command line is read decides, together, how input is received, whether plays are saved and which session overrides apply: devices (Raw Input, saved, the configured keys), autoplay (generated input, unsaved, the built-in keys), or, in development builds, a scripted input log (saved, the configured keys).

- **Input**: The inputs that hit every tap and long note on time are generated once at startup, before the window opens: each note's key is pressed at the note and released 50 ms later or at the lane's next note, whichever comes first, or at a long note's end; mines are left alone. They play one keyboard source with a built-in key config (the default keyboard lanes, no retry or quit binding) and an input offset of 0, fixed for the session and applied every time a play is planned, so every deviation is exactly 0 whatever `device.toml` binds. `device.toml` keeps its own bindings and input offset: the configured input offset does not reach autoplay plays, and F7/F8 still change and save the setting.
- **Memory**: The events are counted from the chart before anything is generated, pressed soundless notes included. The source queue holds the whole stream, the recording window reserves exactly that count, and the recording reservation ("Recording Capacity") is also charged for the queue, the generated events and the merged sound schedule (below) before any of them is allocated. A chart over the budget is refused with the reason before the window opens. Generation merges the lanes' streams into a vector of the counted size, so it neither sorts nor grows, and the merged schedule is moved into the audio engine rather than copied.
- **Key sounds**: A judgment-driven key sound reaches the mixer after its press is judged, a few periods late, so it could never start on the BGM's frames. An autoplay session schedules its key sounds ahead like BGM instead: the engine's BGM list is the chart's BGM merged in time order with one entry per pressed note that has a sound (taps and long note heads at the note time, with the sound the evaluator plays for an on-time press), BGM first and then key entries in lane order at equal times, and the session sends no judgment-driven key sounds. The audio offset applies to them exactly as to BGM. The merged schedule keeps the mixer's pending-start limit and its overload behaviour, so a key sound starts on the same frame as a BGM sound at the same chart time only while the starts fit under that limit; those that do not start at the next wake's first writable frame, as BGM does.
- **Input thread**: Autoplay input has its own input thread: the same 1 kHz high-resolution timer under MMCSS as the Raw Input thread, without Raw Input, so no input device is received. At each wake it pushes every generated event that is due, as batches, before it publishes, so no event lands below a published watermark (the "Watermark" subsection), and it checks the focus gate whether or not recording is open, so a focus loss during the audio tail is seen. It keeps one source set for the whole session, so the focus latch the window holds stays valid across plays. Each play feeds the generated input from its first event, on the generated source disconnected and connected again, which releases whatever an aborted play held.
- **Lifecycle**: Retry, quit, closing the window, the settings keys, focus loss, suspend and audio output failure act as in any play; retry starts the generated input over. Input devices connected or removed have no effect: none is received, and any the source set knows take no part.
- **Nothing is saved**: An autoplay session has no record sink: no input log, no `.measure.toml`, no result is kept. Each play is still recorded in memory and re-evaluated, so the end screen shows its result, with `AUTOPLAY: NOT SAVED` in place of the saved line and without a recommended input offset, and the console prints `autoplay: not saved`. The settings keys' values are still written, as settings, and the log file still records the run ("Log File").
- **Headless runs (development builds)**: `--auto --dev-headless` runs the same autoplay path with a recording of a simulated audio device and WARP offscreen, for tests. `--dev-headless` needs `--auto` or `--dev-perfect`, and `--auto`, `--dev-perfect` and `--dev-scripted-input` exclude one another.
- **Perfect measurement (development builds)**: `play --dev-perfect <chart>` and `measure play --perfect <chart>` measure a play in which every note is hit on time, which a person cannot give a heavy chart. They generate, feed and plan the input exactly as autoplay does (the built-in key config, an input offset of 0 for the session, a recording window of exactly its events), but the engine plays the chart's own BGM and the key sounds are sent from judgments as in a device play, so press → sound and the mixer's figures are those of the judged path. The play is saved with its `.measure.toml` (receive method scripted, development provenance), the end screen shows no recommended input offset, and the console says the input was generated. `measure play --perfect` adds the PresentMon counts as for any `measure play`.

## Audio

- The standard output is low-latency event-driven WASAPI shared mode. The supported period range (integer multiples of the fundamental period) is queried with `IAudioClient3`, and the shortest period that runs stably is chosen. If initialization with a short period fails, it falls back to normal shared mode. When the shortest period is also the default one, normal shared mode is used at that period, with a buffer wide enough to queue three periods ("Path from Press to Sound"). In shared mode, OBS and the like can capture desktop audio and other apps' audio as is.
- Exclusive mode is an optional setting for those who want to cut latency further. The settings screen states that "other apps' audio may stop playing on that output device, and the streaming software's normal desktop audio capture cannot capture the game's audio". ASIO is also an optional setting, and the screen states that whether it can coexist with other software depends on the device and the audio routing.
- The audio thread waits on the WASAPI event handle and writes the next buffer each time it wakes.
- The mixer is our own. Only the key sounds referenced by the chart are decoded to PCM before play starts, converted to the device's sample rate, and kept in memory as i16. Files with identical contents are merged into one.
- Every sound, including a whole song's BGM in a single file, is decoded and kept in memory within fixed caps: each file at most 128 MiB, each sound at most 10 minutes of frames at the device rate, and the whole bank at most 512 MiB of decoded i16. The caps are checked against the file size and the frame count the file declares before decoding, and against a running count while decoding. A chart that exceeds them is refused with the reason. Playing long sounds while reading them ahead on a separate thread is built when a chart the player wants to play is refused.
- At most `min(available cores, 4)` decode workers run. Each reads one file, hashes it, drops it at once if the same contents were already taken, and otherwise decodes it and drops the raw bytes before taking the next. A sound being decoded is charged to the bank cap for the PCM it has allocated, before each allocation: its source buffer (allocated exactly when the file declares its length), both buffers while one grows, and the source and converted buffers together while converting. The charge becomes the stored allocation on success, and it and the sound's declared share are returned on failure. So the caps bound the raw file bytes in flight (workers times the file cap) and the decoded PCM (the bank cap). The decoder's own working memory is not bounded (the "Threat Model" section). Sample rates outside 8–384 kHz are not decoded (the sound stays silent with a warning), which bounds the resampler's buffers.
- The real-time audio processing (callback) does neither loading nor decoding. BGA decoding and read-ahead of long sounds run as bounded work on separate threads.
- Each #WAV id sounds at most once at a time, as BMS charts are written for: starting an id again, as BGM, as an autoplay key sound or as a judged press, stops its earlier instance at the new start's first sample, without a fade. Starts of one id at the same sample sound once. Different ids overlap even when they name the same file, which the bank stores once. Polyphony is capped at 256 voices: a start that finds every voice busy steals the oldest (earliest start), which fades out, and the steals are counted ("Latency Figures"). The cap is kept low on purpose. Ordinary charts stay under it; a chart that goes past it sounds hundreds of overlapping sounds at once, a saturated wall of sound that only strains the player's ears, so cutting the oldest also protects the player; and the busiest periods' mix time grows with the voices sounding (in headless runs of a stress chart on the development PC, 2.7 % of periods took over 1 ms to mix with 1,024 voices, against 0.1 % with 256). It is raised only for a real chart whose steals are heard.
- For sounds that play as BGM, the playback position is computed from chart time and scheduled on the mixer with sample precision.
- BGA (video) is processed on a decode thread, and its display is aligned to the judgment timeline.

### Path from Press to Sound

Key sound latency is the sum of the following segments. Each segment is made measurable, and what can be expected depends on the output path. The device period granted by the audio device is recorded with every measurement, and the game measures and displays the actual latency of the path in use.

- **Standard path (WASAPI shared mode)**: The latency follows from the shared-mode device period P. From judgment to the scheduled mix sample, it is about 2P for the two device periods queued ahead of playback (with one period queued, audio dropped out repeatedly), plus P/2 on average for the wait until the next period event. On a device where a play underruns after its countdown with two queued periods (a driver whose block size beats against the engine period can lock into such a phase), plays from the next one on queue three, which adds P; the choice is kept in the device settings, shown with the latency figures, and lowered only by the player. The mixer resamples nothing during play (the "Aligning Audio to the Judgment Timeline" subsection), so no filter delay is added; a drift-correcting resampler, once built, adds its filter delay (about 1 ms). The OS audio engine and the device's output latency come after this, and the device and input-receipt segments come before judgment. Stage 0 measured a device whose shared-mode period is fixed at 10 ms (`IAudioClient3` offers no shorter period on it), with such a resampler in the path: judgment to the scheduled mix sample was about 26 ms at p50 and 32 ms at p99, which the parts above and that filter delay account for, and no further delay was found. This is how shared mode works on such a device, not a defect.
- **Low-latency path (later stage)**: The 10 ms or less from press to sound applies to a low-latency path built in a later stage: exclusive mode, ASIO, or a device that grants a shorter shared-mode period. Exclusive mode and ASIO stay as described in "Audio" and "Settings Profiles" and are not built before then.

| Segment | What it is | How it is kept down |
| --- | --- | --- |
| Device | Controller scanning and the USB report interval | Depends on the device. Recorded as a device setting |
| Input receipt | Until the OS delivers the input | Receive Raw Input on a dedicated thread |
| Evaluation | Logic thread wake-up and judgment | Wake it immediately on input publication. Judgment processing time is measured, targeting the µs scale |
| Handoff to audio | Waiting until the next buffer write | Lock-free queue. Shorten the buffer period |
| Mixing | Summing, sample rate conversion, fixed headroom and a static soft clipper | Count the conversion latency in the press-to-sound figure; the headroom and the clipper work sample by sample and add none |
| Output | From the written buffer, through the OS and driver, until it sounds from the DAC | Short period in shared mode (standard), exclusive mode (optional). Either way, measure and display the actual latency |

If a "sound to play" event arrives too late for the current buffer write, the sound starts from the first sample that can be written next. The head of the sound is never cut off to make up for the delay. Separately from measuring internal wait times, the actual time from press to sound is also verified with a microphone or dedicated measurement.

## Rendering and Skins

- The play screen is drawn by our own render loop with wgpu. Tool-like screens such as settings may overlay egui.
- The presentation mode (vsync on or off, `present_mode`) and the number of frames rendered ahead (`desired_maximum_frame_latency`) are settings. The defaults favor lower latency.
- Note positions are computed from the time the frame is expected to appear on screen, not the time it is rendered. The expected time is predicted from QPC and the display latency offset, and DXGI frame statistics are used as an aid when valid (the "Time Base (QPC)" section).
- The logic thread publishes a typed state snapshot (for example `PlayView`) through a triple buffer. The render thread only reads it and never writes.
- The play screen today is `SimpleSkin`, the one implementation of the skin boundary `render::PlaySkin`: a hard-coded 16:9 layout authored at 1920×1080 and scaled, drawn with one instanced sprite pipeline (alpha and additive blending) over a texture atlas, and text in a 5×7 ASCII bitmap font baked into the atlas at startup. The declarative skin of stage 3 is a second implementation behind the same trait. Japanese text (`cosmic-text`, `glyphon`) and `egui` arrive with song selection and settings screens, which first need them; until then key config is drawn with the play screen's sprites, and hi-speed, volume and display offset are keys during play. There is no BGA yet; it returns with stage 3, starting from the stage 0 spike's decoder.
- A tap disappears when it is judged; a long note never does. It is drawn until its end passes below the judgment line on the display timeline, and once its head's time has passed, the head stays on the line and the body shrinks. Its body has two parts that a skin draws as it chooses (it may make the inactive one transparent): active while its head has been pressed (held, or decided at a press before its end, BAD included) until its visual hold ends at the release of its lane or its end on the logic timeline, so a tail still on screen after the logic end (a negative display offset) is drawn inactive; inactive otherwise (head missed, released, never pressed). The state follows the events alone (`LnHeld`, a head's judgment at a press before its end, `LnHoldEnded`), never the judgment's value. `SimpleSkin` draws the inactive body darker.
- Short effects such as bombs and the judgment display can be missed with the snapshot alone. Sequence-numbered judgment events are also passed to the render thread through a queue, and the render thread receives everything it has not yet processed as effects.
- Conversion to LR2 IDs or beatoraja property IDs happens only inside each skin's adapter. The game's internal structures are not shaped to fit a skin's ID system.
- Skin buttons and sliders do not modify state directly; they send commands.
- **Screen resolution and aspect ratio**: Output resolutions from FHD to 4K are the realistic range. The render resolution is independent of the display: players may run below the native resolution to keep performance stable, and it is scaled to the output. Only 16:9 is supported for now, because skins are authored for it, but layout and skin coordinates are not hard-coded to 16:9: a skin declares its base resolution and aspect ratio, so other ratios such as 21:9 can be added later by skins designed for them.

### Custom Skin Format

The standard skin does not use a scripting language with GC. It uses a declarative format with no scripts.

- Layout, images, animations, and display conditions are written as data (TOML, etc.).
- Numbers and conditions are written as small expressions that reference `PlayView` fields (e.g. `combo >= 100`, `gauge.value`). Expressions are type-checked and compiled at load time, and per-frame evaluation does not allocate memory.
- **Branching by settings**: The skin declares options (display toggles, replacement images, position adjustment values), and depending on the player's choices, parts are included or omitted and images are switched at load time.
- **Repetition**: Definitions where the same shape repeats, such as the lanes for 7 keys, number digits, and sequentially numbered animation frames, are made with templates and repetition specifications.
- When a visual behavior that the declarative format cannot represent becomes necessary, it is first handled by adding expressions or parts. No general-purpose scripting is added.

The main reason beatoraja uses Lua is to do these two things (branching by settings and generating repetition) at load time. Both are load-time processing, so building them into the declarative format removes the need for both scripts and GC. Free-form per-frame computation is limited to what expressions can cover.

### beatoraja Lua Skins

- Lua is used only inside the adapter for reading beatoraja Lua skins, on the loading thread.
- Lua is run once with the skin settings, and once the returned definition has been converted into the custom skin's declarative format, the Lua state is discarded. When the settings change, the conversion is redone. Lua does not run during play, so Lua's GC does not affect play.
- Lua functions that are expected to be called every frame are analyzed and converted into custom skin expressions. Functions that cannot be converted (those with internal state, those that read the clock, etc.) are treated as constants, and a warning is issued at load time.
- A tool is provided that tries the conversion on commonly used skins and lists the functions that could not be turned into expressions, and it is used to decide on widening the range of expressions supported.

The render CPU work per frame (not just the average, but also the slowest frames) and the latency from input to judgment are measured and displayed; the presented-frame interval distribution is not built yet ("Latency Figures").

## Performance

Performance is not something optimized all at once at the end, but a constraint kept from the start by doing what game development normally does: bounding per-frame work by structure, keeping hot paths free of allocation, keeping heavy work off the render and audio paths, and presenting in step with the display. For now it is verified on one development PC with a WQHD 240 Hz display. Other PCs, other refresh rates, and running alongside other software (streaming and recording software included) are verified later, as variations ("How We Build It").

The figures below are indicators tracked over time, not targets. When an indicator points to a problem a player would notice, its cause is investigated, and then either a mechanism prevents it or it is documented as a limitation. An indicator is never a reason to stop work, and no threshold is set without a reason grounded in what the player sees. Maximums cannot be guaranteed on Windows, so the maximum is an indicator for investigating causes.

### Indicators

| Indicator | What it watches | Orientation |
| --- | --- | --- |
| Frame pacing | Refresh opportunities lost and the longest single loss, counted in whole refreshes ("Frame Pacing in Refreshes") | None lost |
| Render CPU work (including skin evaluation and BGA upload commands) | Render thread time per frame | Well inside one refresh (4.17 ms at 240 Hz) |
| GPU time | GPU work per frame, with the GPU's performance state recorded ("GPU Power State") | Well inside one refresh |
| Input to judgment | From input receipt to the judgment event | On the µs scale |
| Press to sound | Shared mode: judgment to scheduled mix, about 2.5 device periods plus resampler delay, with the device period recorded ("Path from Press to Sound") | As short as the output path allows |
| Audio dropouts | Periods the device played without new audio | None |
| Song selection to play start | Load time, first load and retry separately | 1–2 seconds (Feel Targets) |

Judgment and audio run on threads separate from rendering, so the score does not change even if rendering runs late. But the look of the notes gets disrupted, so rendering is watched with equal weight.

### Frame Pacing in Refreshes

A display with a fixed refresh rate can only show a new frame on a refresh, so frame pacing is counted in whole refreshes rather than as a percentile in milliseconds.

- **Applicability**: The count applies to a fixed refresh rate with vsync: VRR off, tearing not allowed, and either no frame rate cap or a cap paced as a whole number k of actual refreshes (counting vblanks, not a timer at the nominal rate). In any other configuration (VRR, tearing, a cap paced by a timer or not a whole number of refreshes) it is not applicable, and results and the in-game display show N/A, never zero. The standard profile uses VRR when available, so measurements run with VRR off; how to watch pacing under VRR is an open question.
- **Refresh period T**: One refresh period of the display mode actually in use, from its exact refresh rate (for example 4.165 ms at 240.085 Hz). Modes offered as 60 Hz or 240 Hz are rarely exact; the nominal 1/240 s (4.17 ms) is only for orientation.
- **Target interval**: k refreshes of the actual mode, where k = 1 without a cap and, with a cap, the whole number of refreshes per frame the cap is configured for (a 120 fps cap on a 240.085 Hz display is k = 2). k is fixed for the run and does not follow what was observed: an interval of 3 refreshes at k = 2 loses one opportunity.
- **Spans**: Counted over the whole play span, from play start to play end, and over each measurement window inside it.
- **Evidence**: For each span [a, b), the display capture (PresentMon) must contain the span's predecessor (the last displayed frame before a), its successor (the first displayed frame at or after b), and every present the game issued in between. Without that coverage the span's figures are unavailable and shown as unavailable, never as zero.
- **Expected opportunities**: The target grid of a span is anchored at its predecessor's display time p: the times p + j × k × T inside [a, b) are its expected opportunities, the denominator when lost opportunities are given as a share. A span with none has no share and shows it as unavailable. Lost opportunities are clipped to the span as below, so the share is approximate at span edges; it is an indicator, and the lost count itself is exact.
- **Counting a displayed interval**: Only displayed frames count, in display-time order, starting from the span's predecessor, so the interval leading into the span is charged like any other. The interval Δ between two consecutive displayed frames counts as n refreshes, n = floor(Δ ÷ T + 1/2) (a half rounds up). It loses L = ceil(n ÷ k) − 1 opportunities when n > k, otherwise none (at k = 1 an interval of n refreshes loses n − 1). The lost opportunities are the times d + j × k × T for j = 1 … L after the earlier frame's display time d, and only those inside the span count toward it, so an interval that crosses a span boundary is clipped. Because the capture reaches the successor, the last interval of a span is always complete, and a freeze at the end of a span is charged when it ends or leaves the span unavailable.
- **Off-grid intervals**: An interval with |Δ − n × T| above a timestamp tolerance of 0.5 ms is off-grid and counts as one more lost opportunity, in the span that contains its later display time. The tolerance is grounded in stage 0: timestamps of displayed frames scattered by less than 0.1 ms around whole refreshes, while a real miss is a whole refresh (4.17 ms at 240 Hz), so any tolerance between the two classifies the same intervals. The off-grid count is reported; a non-zero count means the display was not on a fixed grid (VRR, composition) or the capture is suspect, and the data is inspected before the lost-opportunity figures are read.
- **Never-displayed presents**: A present that never reaches the screen is not part of the count. When it leaves a frame on screen longer, the displayed interval already carries the loss; when the next present replaces it on time, nothing visible was lost. Never-displayed presents are counted separately as a diagnostic.
- **Recorded figures**: The lost opportunities per span and their share of the expected opportunities, the longest single-interval loss over play, the off-grid count, and the never-displayed count are recorded with every measurement, and the in-game measurement display shows them.

The reason for whole refreshes is timestamp jitter. In stage 0, with the GPU held in its top performance state, the p99 interval exceeded T by 0.03–0.07 ms in every run while no refresh was missed, so a percentile in milliseconds at T flags jitter alone, and it also hides how long a stall lasted. Counting lost opportunities ignores the jitter and counts every refresh a stall covers.

In stage 0 on the development PC at 240 Hz, play lost no refresh opportunity at the top performance state. Under the driver's default power settings it lost single intervals of 1–2 refreshes, frames whose GPU work ended after the vblank ("GPU Power State"). Runs at 60 Hz and with streaming software are kept for the later verification of those variations.

### Bounding Work by Structure

- Per-frame work is made proportional to what is visible on screen, not to the size of the chart. Only visible notes are drawn, and only visible skin elements are evaluated.
- Input, logic, the audio callback, and per-frame render work do not allocate memory during play (the scope is defined in the "Key Libraries and Algorithms" section).
- Sprites are batched into a few draw calls with texture atlases and instanced rendering. No draw call is issued per note.
- Text uses a glyph atlas built in advance, and there is no per-frame typesetting.
- egui is not used on the play screen (because it rebuilds the UI every frame). egui is limited to settings screens and the like.
- The input thread wakes at 1 kHz with a high-resolution waitable timer (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`). The input and audio threads have their priority raised with MMCSS (`AvSetMmThreadCharacteristics`). Only these two threads get raised priority (see below).
- If BGA decoding falls behind, that frame is skipped, without affecting audio or judgment.

### Display Latency and Stutter

Even if the CPU and GPU times are short, presentation is not necessarily stable. The following points are decided and verified.

- **Presentation mode**: Use the DXGI flip model with a waitable swap chain and 1 frame rendered ahead, a low-latency path even in borderless display. Vsync on or off, variable refresh rate (VRR), and allowing tearing are settings, and which path wgpu actually uses is verified.
- **First-time stutter**: Shader and pipeline creation and the first texture uploads are all completed before the countdown. Uploads are done a little at a time through a bounded queue.
- **Measurement**: The actually presented frame intervals are measured with PresentMon or similar. First load (no cache), retry, real controllers, and the GPU's power management setting and observed performance state are also included in the measurement conditions; slowdowns from laptop power saving and heat join them when laptops are supported.

### GPU Power State

In stage 0 the development PC's discrete GPU stayed in low performance states (P5 and P8) during play under the driver's default power settings, and at 240 Hz some frames' GPU work ended after the vblank and lost a refresh. With the driver's power management set to maximum performance it held its top state (P0) and no refresh was lost. The development PC runs with that setting, and every measurement records the setting and the observed performance state.

How games usually keep the GPU out of low performance states is taken from common game development practice when it matters, rather than investigated here. It is an open question.

### Memory Budget

Budgets are set for each of decoded audio (about 115 MB per 10 minutes at 2 channels, 48 kHz, i16), temporary usage during loading, textures, and BGA display surfaces. The number of parallel decodes is capped. If it is known before play that the budget will be exceeded, it is brought within budget by switching to read-ahead for long sounds or lowering BGA quality; if it still does not fit, the reason is shown and play does not start.

The chart's display geometry is fixed at load time and bounded by the source: an `i128` position per note head, long-note end and bar line, and a 48-byte scroll segment (an `i64` start, an `i128` anchor and slope, aligned to 16 bytes) per BPM change, STOP start and STOP end.

### Recording Capacity

Input capacity and the memory behind it are fixed when recording opens, from the participants it opens with and the length of the recording window: countdown and lead-in, the chart, and finalization through the final deadline (the chart duration is capped, "Loading Limits").

- **Inputs**: each lane-bound button gets room to be pressed and released 30 times a second over the window, at most 240 edges a second for all of one source's buttons together, and each lane-bound axis its source declares 1000 events a second. A system control (retry, quit) gets a press and a release in all, since its first press ends the play, so a keyboard bound only to retry and quit adds 4 inputs whatever the window's length, and the many keyboard handles Windows exposes do not grow the window. These rates size one capacity shared by every recorded control; none is enforced on its own, so a burst fits while the window's total does and the source's queue (4,096 events) has room. A retry or quit key held across a retry auto-repeats in Raw Input while the window ignores the repeats, and may use up the capacity.
- **Overflow**: input beyond the capacity aborts the play at the last finalized watermark ("Play Lifecycle"); a full source queue drops the incoming event while the play goes on. Either marks the log incomplete with the reason "overflow".
- **Reservation**: a play reserves its whole recording window right after recording opens, before the play is handed to audio and the screen, never less, within a budget of 512 MiB. The policy (`game::Reservation`) counts what the constructors allocate: per input of the capacity, the evaluator's input ring, each source's staging, the merge and the record, and the evaluator's maximum output per input (2 + gauges events) for the output, the audio commands and the live copy of the events; and, fixed, the chart's share of those events, the render effect queue, 64 bytes per chart object for the evaluator's tables and 64 KiB for small parts. With one keyboard and two gauges that is 680 bytes an input, about 790 thousand inputs: 54.8 minutes for a keyboard alone. Each further source adds its staging, 24 bytes an input: a controller with seven buttons and an axis reporting without pause plus one keyboard record about 10.25 minutes. A window that needs more is refused there, with the inputs it needs and the inputs that fit; recording closes again and nothing is played. The budget keeps every record within what the standard decoder reads at the worst-case 22 bytes an event.
- **Touching**: reserved memory is not touched in advance. Its pages fault in as the buffers fill during play, in proportion to what is written rather than to what is reserved; the logic step's output and merge buffers, cleared every step, use pages up to the largest step. The process's page faults over each play are counted in `.measure.toml` ("Latency Figures"). Touching the reservation in advance would make all of it, up to the budget, resident in every play and add that much writing to every retry, while only faults the play's own buffers explain, alongside latency outliers, would justify it.
- **Known limits**: about 10 minutes with a reporting turntable axis, because the output is reserved for the worst case of every input rather than streamed out in bounded steps (#43); and an axis reporting faster than 1 kHz, which can exhaust the capacity within a play of ordinary length: in a three-minute play with nine buttons and one axis that changes on every report, the play aborts once the axis has moved for more than about 15 % of it at 8 kHz, 30 % at 4 kHz or 60 % at 2 kHz (#45). Such polling rates are mainstream for mice and keyboards, not for dedicated controllers, and polling rate does not raise the cost of buttons or keys. Reserving 8 kHz per axis under this reservation (about 650 MiB for two minutes) would refuse ordinary axis plays; a larger capacity waits for bounded output streaming, with the replay decoder bound kept as its own admission check.

### Loading and Song Selection

- Key sounds are decoded in parallel on multiple threads. Converted PCM is cached on disk, keyed by file hash, conversion settings, and decoder version. Retries reuse the data in memory. First load and retry are measured separately.
- A library of tens of thousands of songs is stored in SQLite, and only files whose modification time or size changed are re-read. Search uses FTS5, and scanning runs in the background.

### Measurement

- In game, the frame time distribution (p99 and maximum), the lost refresh opportunities (N/A where that count does not apply), GPU time, the latency from input to judgment, and the number of audio dropouts can be displayed at any time ("Latency Figures" lists what is built).
- A benchmark scene plays back a fixed input log with the worst-case chart and skin, and it is measured regularly on the development PC.
- CPU-side time (evaluator, skin evaluation, loading) is measured as benchmarks (criterion) in CI to detect large regressions. Measurements on shared CI fluctuate, so absolute values are tracked on the development PC.
- For detailed investigation, per-span times are inspected with Tracy (`tracy-client`).

### Latency Figures

Each physical input event carries its raw receive time. The evaluator copies it, unchanged and not offset-adjusted, into the events an input produces as `origin` (none for deadline events), and sound commands and render effects carry it on. This is data passed through; the evaluator still reads no clock. `origin` is used only for subtraction, never as an identifier: every event of one batch shares it, so the judgments of a chord have one origin, and events are identified by their sequence number.

An overlay toggled during play shows p50, p99 and max over the current play, from preallocated whole-play histograms (exact below 64 µs, then 32 buckets per power of two, so a percentile is within 1/64 of the exact one for values below about 134 s; count and max exact), so nothing allocates. The same figures are written next to each saved record as `<record>.measure.toml`, numbers only, a figure without evidence left out.

| Figure | How | Notes |
| --- | --- | --- |
| Input → judgment (µs) | Logic processing time − origin, per judgment a press made (a long note held at its press counts there; a long note judged by its release or end gives no sample) | |
| Finalization delay (µs) | Logic time − watermark at each advance | |
| Press → sound scheduled (ms) | `MixClock` time of the scheduled start sample − origin, shown with the device period and queued periods | Internal path only; output after the engine is not included |
| Press → on screen (ms) | Per judgment a press made (as for input → judgment), by its sequence number: display time of the first presented frame that drew it − its origin. Display time = `SyncQPCTime − (SyncRefreshCount − PresentRefreshCount) × refresh period`, from the statistics sample whose `PresentCount` matches that frame. Samples where `PresentCount` did not advance, `SyncQPCTime` is 0, the refresh gap exceeds 4, or the call failed are rejected | N/A when the frame has no accepted sample. Never predicted, because a prediction would measure itself |
| Lost refreshes (frame statistics) | `PresentRefreshCount` deltas between accepted samples of consecutive present counts, counted with the rules of "Frame Pacing in Refreshes" at k = 1. An interval without accepted samples at both ends is N/A, never lost | Diagnostic; N/A under VRR or tearing. PresentMon (`tapestry measure play`) is the recorded evidence |
| Render CPU work per frame | QPC around update, build and submit, waits excluded | |
| Audio underruns | Wakes with zero padding | |
| Sprite cap | Play frames whose sprite batch filled (8,192 sprites), so the rest of the frame was not drawn | Frames, not sprites: the skin stops building at the first refused push, so the HUD, effects and this overlay vanish from such a frame too |
| Deferred starts, stolen voices | Over the play: audio wakes that left a due key sound or BGM start for a later wake because 1,024 starts already waited (those sounds start late, head intact), and voices stolen at the 256-voice limit | Neither is reported anywhere else |
| Play setup, countdown first frame (µs) | From the countdown's start (recording opened, chart time −lead-in) to the play's handover to audio and the screen, and to the clock read once the play's first frame was presented | Printed as the `countdown:` line at the play's end too. A setup longer than one refresh costs the countdown a frame |
| Page faults (process) | `PageFaultCount` of `GetProcessMemoryInfo`, from just before the play's handover to audio and the screen until the play is over | Diagnostic: the whole process, every thread, wgpu and the driver included; Windows reports no per-thread count. The play's reserved buffers fault in as they fill ("Recording Capacity") |
| Load time | First load and retry, separately, each to the clock read once the play's first frame was presented (not its display time): the first load from the process start (under `measure play`, from when its PresentMon capture has settled), a retry from its receipt | |

At each play's end the console prints the sprite cap, deferred-start and stolen-voice counts in one `limits:` line, since an autoplay play keeps no `.measure.toml`.

GPU time and the presented-frame interval distribution are not built yet. The current play screen is light, and lost refreshes already show any presentation stall; GPU timestamp queries arrive with the effects of stage 3, which add GPU load. Until then PresentMon's capture holds the presented intervals.

### Coexisting with Other Software

Many people play while streaming or recording, so the game does not, for the sake of its own latency targets, monopolize the PC's resources and significantly impair other software (OBS, browsers, voice chat software, etc.). The following rules are for the standard settings. They are followed from the start; verifying them with other software running is a later variation ("How to Verify Running Alongside OBS").

| Resource | Rule |
| --- | --- |
| CPU | No busy-waiting (spin loops); every thread sleeps on events or waitable timers. Only the input thread during play wakes at 1 kHz; when not playing, it uses longer intervals or stops. Process priority stays normal, and only the short work of the input and audio threads is registered with MMCSS. Background work has a parallelism cap, and heavy work (library scanning, etc.) is paused during play. CPU affinity is not pinned |
| GPU | Never render beyond the display refresh rate. With VRR, the cap is set slightly below the top of the VRR range. GPU queue priority stays normal. The GPU can be saturated even when capped at the refresh rate, so the streaming settings lower the cap and lighten the effects |
| Audio | Use shared mode so as not to interfere with other apps' audio or with capture by streaming software (the "Audio" section) |
| Display | Use the borderless flip model so that OBS game capture and window capture can capture it |
| Input | Do not use methods that monopolize input (exclusive DirectInput, low-level keyboard hooks). Do not interfere with other apps' hotkeys (such as controls for streaming software) |
| System-wide settings | Do not change the timer resolution request (`timeBeginPeriod`), the power plan, or other processes' priorities. Avoid unnecessary timer requests, and get the needed precision from high-resolution waitable timers (waitable timers do not guarantee the wake time either, so the latency is measured) |
| Disk and memory | Loading and library scanning run on low-priority threads (with lowered I/O priority as well). Memory stays within the "Memory Budget" |

### Settings Profiles

The rules above are a commitment for the standard settings. Only when players choose it themselves may exclusive methods be used to cut latency. The first version builds only the standard profile; the low-latency priority and streaming profiles are later variations.

| Item | Standard | Low-latency priority | Streaming |
| --- | --- | --- | --- |
| Audio | Low-latency WASAPI shared mode | WASAPI exclusive mode, or ASIO | Shared mode |
| Display and sync | Borderless flip model. VRR if available, otherwise vsync | Choose from combinations of: VRR, allowing tearing (no vsync), exclusive fullscreen | Borderless flip model, vsync |
| Frame rate cap | Display refresh rate (with VRR, slightly below the top of the range) | May exceed the refresh rate only when tearing is allowed | Below the refresh rate (for example 60 or 120) |
| Effects and BGA | Standard | Standard | Lighter |
| Game-side target | Display refresh rate | Display refresh rate, or the configured cap | The configured cap. OBS stability takes priority |

- In every profile, system-wide settings are not changed, there is no waiting in spin loops, and input is not monopolized. These barely help shorten latency, yet affect the whole PC.
- When low-latency priority is chosen, the settings screen shows the following: in exclusive mode, other apps' audio may stop playing on that output device, and the streaming software's normal desktop audio capture cannot capture the game's audio. Whether ASIO can coexist with other software depends on the device and the audio routing. Exclusive fullscreen does not necessarily reduce latency.
- A profile is a starting point; items can be changed individually.

### How to Verify Running Alongside OBS

Running alongside OBS is verified later, as a variation, once the game can be played to the end on the development PC. What stage 0 found to build on:

- **Same GPU**: OBS game capture needs OBS and the game to render on the same GPU. With the game on one adapter and OBS on another, capture failed, and its repeated re-hooking disturbed the game. Streaming guidance tells players with two GPUs to run both on the same one.
- **Conditions to fix when it is verified**: The game's output and render resolutions and refresh rate, the OBS scene (game capture plus about one browser source), the OBS output (1080p, 60 fps), the encoder and its settings, and a measurement of 10 minutes or more including the heaviest chart. The state of Windows Game Mode is left unchanged and recorded.
- **What is watched**: On the game side, the same indicators as without OBS. On the OBS side, the share of frames lagged due to rendering (lagged ÷ attempted drawn frames) and of frames skipped due to encoding lag (skipped ÷ total frames).
- The streaming settings exist to lighten the game when either side shows problems.

### Settings for Low-End PCs

Quality settings such as lowering the BGA resolution or turning the BGA off, and reducing effects, are provided so that PCs slower than the development PC can keep their refresh rate. They are tuned when those PCs are supported.

## Extension Boundaries

Extensions can plug in only at named extension points. Evaluation has no plug-in point.

| Extension point | Kind | Example extensions | Constraint |
| --- | --- | --- | --- |
| Ingestion | Handler | Chart format adapters (BMS, PMS, BMSON, etc.) | Outputs the common chart model |
| Chart transformation | Transform | RANDOM, MIRROR, etc. | Randomness can be used only when the seed is recorded |
| Input | Transform | Input device adapters | Passes events normalized to timestamped events |
| Evaluation | — | None (changed only through preset data) | No plug-in point |
| Session policy | Decision | GAS, handling of failure | Falls back to the default policy if the extension fails |
| Output | Receive only | Audio, skins, statistics | Cannot write to the evaluator or to audio |
| Screen transition and session events | Receive only | OBS control, Discord presence, streaming overlays | Receives on a separate thread, and can neither stop nor delay the game |
| Results | Transform | Analysis screens, re-evaluation under other presets | Reads the input log and the judgment event stream |
| Saving | Receive only | Sending to IR, export | Even on failure, the play record remains locally |

### What Stays Out of Core

| Feature | Reason | Where it lives |
| --- | --- | --- |
| Chart formats | Formats multiply, but all can be converted to the common chart model | Ingestion adapters |
| Skins | The structure differs greatly per compatibility target | Skin adapters |
| Input devices | Devices differ from one another | Input adapters |
| Statistics and analysis screens | What people want to see differs from person to person | Result extensions |
| Integration with streaming and external tools (OBS, Discord, etc.) | Each integration target has a different communication mechanism, and targets increase and change | Extensions that receive screen transition and session events |
| IR and external integration | Destinations increase and change | Saving extensions |
| Chart distribution | The player itself works entirely from local folders | A separate service and installer |
| Converting commercial game data | It would aid piracy | Nowhere |

Conversely, judgment logic is placed in core and fixed, to preserve fairness and reproducibility. Rule-specific evaluation implementations are only those built into the evaluator, and no mechanism is built for users to extend judgment rules with code.

The extension points exist from the first version as named interfaces in code, and core runs through them; what is kept to a minimum is the extensions plugged into them. The first playable version (stages 1–3) plugs in only what it needs to be played to completion ("How We Build It"), and the rest are added one at a time later:

| Extension point | Plugged in for the first version | Added later |
| --- | --- | --- |
| Ingestion | BMS/BME adapter, including `#RANDOM`, writing the common chart model | PMS, BMSON, and other formats |
| Chart transformation | None (the stage exists between ingestion and evaluation and passes the chart through) | MIRROR, RANDOM options and the like |
| Input | Raw Input adapter for keyboards and HID dedicated controllers | MIDI and other input adapters |
| Evaluation | — (no plug-in point) | — |
| Session policy | The default policy | GAS and other policies |
| Output | Audio and rendering, receiving the judgment event stream | Statistics and other receivers |
| Screen transition and session events | None | The event stream, the local event endpoint, and integrations built on them (OBS, Discord, overlays) |
| Results | Re-evaluation under other presets (the game's core feature, shipped as the built-in result extension) | Other analysis screens |
| Saving | Local saving of play records and input logs | Sending to IR, export |
| Skins (through the skin adapter) | The declarative custom skin | LR2 and beatoraja skin adapters |

Settings profiles follow the same rule: the first version ships the standard profile, and the low-latency priority and streaming profiles come later.

### Screen Transition and Session Events

Built as a later variation, not in the first version. Core streams changes in screen and play state (song selection, play start, pause, failure, play end, result display) as timestamped events. All integrations with streaming and external tools are built as extensions that receive these events.

- Extensions receive on a separate thread from a bounded queue. When the queue overflows, old events are dropped, and the game does not wait. Even if an extension fails, the game simply continues.
- Events carry a session ID and a sequence number, and the current state can also be fetched at any time. When an extension notices a gap in sequence numbers or a reconnect, it re-fetches the current state. Even if events are missed, re-fetching the state gets back to the correct state.
- Extensions cannot write to judgment, audio, or rendering.
- What is streamed is limited to screen state and play results (song title, chart, preset, score, clear status); the input log itself is not streamed.

### Local Event Endpoint

Built with the screen transition and session events, as a later variation. A generic endpoint (a local WebSocket) that can be connected to only from within this PC is provided, exposing the events above and the current state. OBS scripts, streaming overlays displayed in a browser, and statistics tools can be built on this endpoint without rebuilding the player.

- It listens only on loopback addresses and is disabled by default.
- A connection requires a random connection token issued from the settings screen. Nothing is sent until authentication completes.
- The `Host` on connection is accepted only for combinations of a known loopback notation and the configured port (a defense against DNS rebinding). `Origin` is checked by exact match against an allowlist. The value depends on the loading method; for example, when an OBS browser source loads local HTML, `Origin` is `http://absolute`. So the supported loading methods are decided and checked against the actual values. `Origin` is not a substitute for authentication.
- Connections that are slow to receive also have a send queue cap, and are disconnected on overflow.
- Scene switching and recording start and stop via OBS WebSocket are provided as an official extension built on top of this endpoint. Recording start and stop are handled so that sending them any number of times gives the same result, and recordings this extension did not start are never stopped automatically.
- Discord presence integration sends information externally, so it is disabled by default, and the settings screen shows what is sent.

### Tuning Without Restart

- When preset or skin files change, they are reloaded immediately.
- A reloaded preset is used for play from the next retry on. If the rule changed in the middle of play, the play could no longer be reproduced with a single preset.
- Re-evaluation on the result screen may use the reloaded preset immediately. Combined with `eval-cli`, the cycle of writing a preset, recomputing with local input logs, comparing the results, and fixing it runs without a restart.
- Skin reloading is done on a separate thread, and the fully assembled result is swapped in between plays. This is so that render work during play does not allocate memory.

> **For now: the preset reloads at the next retry**
>
> Files are not watched yet. At each retry the session checks the preset file's modification time, and a changed file is loaded and used from that retry on, which is the earliest point a reloaded preset may be used in play. A file that fails to load leaves the previous preset in use, and the error is reported. File watching, and immediate re-evaluation on the result screen, arrive with the result screen in stage 2.

## Settings Scopes

| Scope | Owner | Examples |
| --- | --- | --- |
| Device | This PC and this device | The three offsets, debounce, polling, audio device, buffer; with their variations, the OBS connection target (the password is kept in the OS credential store) and whether the local event endpoint is enabled |
| Player | The person playing | Hi-speed, lane cover, the presets and skins used, the preferred LN judgment mode, session policy |
| Preset | The person who authors rules | Judgment windows, gauge, `#RANK` conversion |
| Chart | The chart author | `#RANK`, `#TOTAL`, specification of the LN judgment mode (it takes precedence over the player's choice) |

Device and player settings go in separate files, so that latency settings do not get mixed up when several people share one PC or when a device is replaced. There is no team or organization scope. Anything to be shared is distributed as preset or skin files.

Results and replays display and record where the values used came from ("preset X (hash), offsets from the device settings").

### Settings Files

- Settings are TOML in two files: `device.toml` (the keyboard key config, the controller key config with its controller's model, the three offsets, the audio device, master volume, queued audio periods, the monitor) and `player.toml` (hi-speed, the preset file, the displayed gauge). Files are written atomically: to a temporary file, then renamed.
- Everything lives in one data root, `%LOCALAPPDATA%\tapestry\`, which also holds `presets\` (with `custom.toml`), `records\`, `logs\` and `measure\`. Local, not roaming, AppData, because device settings must not follow a profile to another PC and records are large; one root keeps backup simple, and the scopes stay separated by file. The registry is not used: it is not human-editable and does not fit presets distributed as files.

## IR (Rankings)

The custom IR receives input logs (replays), not scores. The server recomputes with the same evaluator and builds the rankings.

```
Client ─send→ { start token, chart hash, chart transformations, device settings, input log, versions }
Server ─store→ store the input log as the source of truth
       ─evaluate→ evaluate() with every registered preset
       ─aggregate→ rankings per (chart, preset, LN judgment mode in effect, evaluator version, metric)
```

### What This Enables

- Whichever preset you play with, you appear in the rankings for all presets. There is no need to settle on a single official preset.
- New presets can be applied retroactively to stored plays.
- When an evaluator bug is fixed, all logs can be recomputed to reflect the fix.
- Metrics other than score can also be ranked: besides per-preset EX score, BP, and clears per gauge type, timing accuracy (the mean and standard deviation of deviation) and so on. Metrics are added to the server over time as pure functions that take the judgment event stream.

### Decisions to Make

- **Ranking unit**: The preset's content hash and the evaluator version. When either changes, a policy decides whether to create new rankings or recompute and replace them.
- **Distinguishing the preset actually played**: Either mark the records actually played with that preset, or separate the rankings.
- **Offsets**: Accepted as legitimate adjustment, with an upper limit on the range decided and recorded.
- **Records under different conditions**: Normal rankings include only records that played the whole chart at normal speed to the end and meet the permitted conditions. Speed changes, assist options, records ended midway, mid-chart starts, section repeats, and excerpted section evaluations are not included in the normal rankings.
- **Chart retrieval**: A retrieval path for chart files (identified by hash) is provided so the server can resolve the same chart. The distribution registry (the "Chart Distribution" section) doubles as that path.

### Server Architecture (Cloudflare)

| Role | What is used |
| --- | --- |
| API | Workers (TypeScript) |
| Evaluator | A WASM module built from the Rust `eval` crate for `wasm32-unknown-unknown`. Called from the API Worker |
| Running evaluation | A Queues consumer Worker. The API stores the input log in R2 and enqueues only a reference to it. The consumer evaluates it and writes to D1 |
| Input logs and chart files | R2 |
| Results and rankings | D1 |

- The evaluator uses no threads, clock, or I/O and computes only with integers, so it runs on WASM as is. The WASM build of the `eval` crate is kept passing in CI at all times.
- The same input log is evaluated with both the local native build and the WASM build, and CI verifies that the results match.
- Evaluation is not done inside an HTTP request. The CPU time and memory of the whole processing of one item (expanding the input log, loading and resolving the chart, passing data between JS and WASM, evaluation, writing the result) are measured and kept within a budget well below the Workers limits. The sizes of input logs, chart files, and the WASM module are also capped.
- Writes to D1 give the same result however many times they run. The number of retries is capped, and failed jobs are recorded separately. A cap on overall intake and a rate cap for recomputing past records are also set. This is because Queues smooth the load but do not limit the total.
- The server needs the chart's contents to evaluate. It receives the chart text file (not including audio or images), and stores it privately only if the SHA-256 computed on the server matches the allowlist and the author has permitted storage for evaluation in the distribution registry. Charts without permission are not evaluated on the server and are not eligible for rankings. The server does not resolve the audio or image paths that charts reference.

### Connecting to Existing IRs

Built in a later roadmap stage, as an extension at the saving extension point. The connection adapter holds "the preset that IR requires", and sends scores computed on the client in the existing format. As long as the input log format has all the fields above, this can be added later.

## Threat Model

### Assets to Protect

- The player's PC (preventing code execution and crashes via charts, skins, or packages)
- The trustworthiness of the rankings
- Accounts and keys
- Privacy (input logs, the times of day of play)
- Service operating costs (Cloudflare billing)
- Legal standing (piracy, relationships with rights holders)

### Threats and Mitigations

| Attack vector | Threat | Mitigation | Stage |
| --- | --- | --- | --- |
| Chart files | Computation breakdown or DoS from a huge note count, extreme BPM or STOP, deeply nested `#RANDOM` | Cap the note count, line count, and time range (`#RANDOM` blocks cost O(1) a line whatever their depth). Time computation is exact with arbitrary-precision integers, caps their size, and rejects loading when exceeded. The same caps are applied on the server | 1 |
| Audio and video | Decoder vulnerabilities | Use maintained decoders and narrow the supported codecs. Accept the remaining risk and keep dependencies up to date. Decoding in a separate process with restricted privileges is decided after measuring whether it is worth the handoff cost | 1 |
| Archives | zip slip, zip bomb | Do not allow writes outside the extraction directory. Cap the extracted size and file count | 1 |
| Dependencies | Supply chain attacks | Run `cargo-deny` and `cargo-audit` in CI and on a daily schedule, check for unused dependencies with `cargo-machete`, and pin the lockfile | 1 |
| Local event endpoint | Connections from other PCs or malicious web pages, exfiltration of information | Listen only on loopback addresses, disabled by default. Authenticate with a connection token, and check `Host` and `Origin` against allowlists (the "Extension Boundaries" section). Input logs are not streamed | With its variation |
| Development features | Abuse of scenario launch or external control left in production builds | Remove the code entirely from production builds with Cargo features, and verify the removal in CI (the "Testing Strategy" section) | 1 |
| Auto-update | Distribution of malicious code to all users | Sign update files and verify them. Use Windows code signing | 2 |
| Skins | Resource exhaustion, reading outside the skin | Expressions in the declarative format have caps on size and evaluation cost. Cap image dimensions and decoded size, and restrict the files that skins and charts reference to within their respective folders | 3 |
| Lua skins | Code execution via Lua, infinite loops | Run Lua only at load time. Build the environment allowlist-style, without `io`, `os`, `dofile`, `loadfile`, `debug`, unrestricted `package`, or module loading. Cap memory, instruction count including coroutines, and calls to host functions | With the beatoraja skin adapter |
| Modified clients | Fabricated input logs | Assume it cannot be prevented, and rely on detection and human review (the "Countermeasures Against Cheating and Piracy" section) | 4 |
| Others' logs | Resubmission of someone else's record | Bind start tokens to accounts. The hash of the submission is only a hint of duplication and cannot identify modified copies | 4 |
| Evaluator version | Claiming an old version with bugs | The server computes only with its own evaluator. The client's version is treated as informational only | 4 |
| Mass submission | Wasting CPU time and billing | Rate limits per account and per IP. A cap on input log size (song length × maximum input rate). Smooth the load with Queues | 4 |
| Accounts | Takeover, impersonation | Hold no passwords and use external login (Discord, GitHub, etc.). Narrow API token permissions | 4 |
| Registry | Registration of pirated content, author impersonation, confusing names | Verify publishers with signing keys, with procedures for key revocation and renewal. Registration is reviewed | 5 |
| Takedown requests | Removal through false claims | Define procedures for requests and counter-notices | 5 |
| Source URLs | Content substitution | Always verify contents by hash, and do not install on mismatch | 5 |

The client source is assumed to be public, and only mitigations that hold while attackers can read the code are adopted. Detection thresholds and secret values are kept only on the server.

Known limitation: loading caps bound the raw sound file bytes in flight and the decoded PCM ("Audio"), but not the decoder's own working memory, such as the codebook tables a crafted Vorbis header sets up. Only the decoder could bound those. A crafted sound file can therefore make loading fail or exhaust memory and crash the game while it loads a chart the player chose; play, records and other files are unaffected. This is revisited with chart import and distribution (stage 5), when charts arrive from people the player does not know.

### Privacy

- Replays are private by default, and the player can choose to publish them.
- Account deletion and data export are supported.
- For the Discord integration, the information sent externally (song title, whether playing) is shown on the settings screen, and it is disabled by default. The OBS integration stays entirely within the local PC.

### Out of Scope

- Kernel-level cheats, and automated input pressed by hardware
- Cheating in offline single-player play

## Countermeasures Against Cheating and Piracy

Neither can be prevented completely. Cheating is handled by stacking layers that make it not worth the effort, and piracy by two points: "do not list it in the rankings" and "do not build features that aid piracy". The client is assumed to be open source, and kernel-level anti-cheat and obfuscation are not adopted.

### Anti-Cheating Measures

| Layer | Mechanism | What it prevents |
| --- | --- | --- |
| Server-side computation | Scores are not accepted; they are computed from the input log | Rewriting just the score numbers, mismatches between the recorded options and the computation. Fabrication of the input log itself cannot be prevented |
| Start registration | A one-time token and a random seed are issued at start. Only submissions within a time consistent with the chart's length are accepted | Clients specifying a seed of their choice to aim for a particular arrangement, reuse of old logs. Feeding automated input live cannot be prevented |
| Impossible input | Input inconsistent with the device's specifications (mashing faster than the polling interval, debounce violations, etc.). HID puts multiple buttons in one report, so matching times of simultaneous presses alone is not used as the basis for a decision | Clues to crude automated input |
| Statistics | Whether the deviation distribution looks human (how it grows with density and chords, fatigue late in the song, comparison with the player's own history) | Narrowing down what to investigate (no action is taken on this alone) |
| Human review | Anyone can play published replays in the client. Top records become "verified" by publishing the replay or submitting a video | Sophisticated cheating |
| Trust levels | New accounts and unverified records are not shown at the top right away | Vandalism with throwaway accounts |

> **Rerolling RANDOM is normal play**
>
> Retrying over and over until a favorable arrangement comes up is treated as legitimate play and is not restricted. Start registration exists to guarantee that "the seed is truly random", and the number of rerolls does not matter. So as not to spoil the feel of retries, tokens and seeds are received in batches in advance, and no retry waits on the network. Play also works as is when offline, and the records are treated as "unverified".

Detection thresholds are kept only on the server side and are not published. Sufficiently sophisticated automated input will eventually become indistinguishable even statistically, so the very top records rely on human review.

### Anti-Piracy Measures

- **Rankings use an allowlist.** Only charts with clear provenance (works from BMS events, charts listed in difficulty tables that manage provenance, charts registered with a signature in the distribution registry) are eligible. A denylist does not work, because changing a single byte changes the hash.
- Charts not on the allowlist can still be played locally. They simply do not appear in the rankings, or are separated as "unverified".
- On a request from a rights holder or author, the chart is removed from the registry and the allowlist. A removed chart also automatically drops out of IR eligibility.
- Users' local files are not scanned. There is no matching against commercial data either (because matching would require holding commercial data as the reference).

## Chart Distribution

A chart registry and an installer are provided as a service separate from the player itself. The registry doubles as the IR allowlist and serves as the record of chart provenance.

### Manifest (One Package = One Work)

- Name, version, author, distribution terms (whether redistribution is allowed, whether mirroring is allowed)
- File list and hashes. SHA-256 and MD5 for charts
- Source URLs, dependencies (for a derivative chart, the base package it depends on)
- Signature by the author or the event organizer (a scheme like minisign)

### Distribution Modes

- **Catalog only (default)**: Fetch from the author's distribution URL and verify by hash that the contents are correct. The lowest operational burden and legal risk.
- **Hosted**: Only when the author consents, we host it. Links do not break even if the event site shuts down.

### BMS-Specific Features

- **Derivative chart dependency resolution**: Installing a derivative chart also installs the base package it needs.
- **Difficulty table collections**: A difficulty table, which is a list of chart hashes and URLs, can be installed in one go.
- **Content-hash storage**: Many derivative charts use the same base, so files are stored by content hash and the same key sounds are not stored twice.

### Import Safety

- Prevent attacks that write outside the extraction directory through paths inside a zip (zip slip), and zips that become enormous when extracted (zip bomb).
- File names inside zips are often Shift\_JIS, so the character encoding is detected before handling them.
- Nothing in a package is executed. Even when Lua skins are distributed, they are used only for conversion in the restricted load-time environment, and are handled one step more cautiously than charts.

When an author fixes a chart, its hash changes. Rankings stay separate per chart hash, and the manifest links the old and new versions. Whether records for the old version carry over is decided by the author or the ranking operator.

## Language and Crate Structure

Rust is recommended as the core language. The evaluator can be shared between the client and the IR server, deterministic computation and testing are easy, and libraries such as Shift\_JIS conversion (`encoding_rs`) are available. Odin is also a strong option: miniaudio, D3D, and Lua bindings come standard, and the game part can take shape quickly. However, it would mean accepting that it is pre-1.0 and has few libraries. The standard skin uses a declarative format with no scripts, and Lua is used only in the beatoraja Lua skin adapter.

| Layer | crate | Role |
| --- | --- | --- |
| L0 | `bms-model` | Chart types, time (µs) types |
| L1 | `bms-parser` | Loading BMS/BME/PMS/BMSON, `#RANDOM`, time computation with rational numbers |
| L2 | `rules` | Preset types, validation and canonical encoding, judgment window and gauge tables. Takes no external dependency; loading preset files (TOML) sits in `game`, so `eval`'s dependency closure stays free of external crates |
| L2 | `eval` | The evaluator and its input contract (physical input events, device profile, start snapshot, watermark and merge), which `input` and `replay` use. Depends only on `bms-model` and `rules` |
| L2 | `replay` | Input log and replay formats |
| L3 | `input`, `audio` | Raw Input/HID/MIDI, WASAPI/ASIO and the mixer |
| L4 | `game` | Logic thread, session policy, publishing the state snapshot |
| L5 | `render`, `skin-*`, `ui` | wgpu rendering, the adapter for each skin, egui screens |
| apps | `player`, `eval-cli` | The player itself, and a headless CLI that evaluates replays |
| Later | `ir-server`, `pkg` | IR server (shares `eval`), distribution registry and installer |

Dependencies go only one way, from upper layers to lower layers. `eval` cannot depend on clock, I/O, audio, or rendering crates, so its purity can be enforced by crate dependencies.

Windows-dependent code is confined to `input`, `audio`, `render`, and `player`. Linux CI verifies that every other crate builds and passes tests on platforms other than Windows. Therefore, no dependency (including indirect dependencies) from those other crates on these Windows-only implementations is created either.

- `input` and `audio` are portable crates with their Windows parts under `#[cfg(windows)]`, and `windows` as a `cfg(windows)` dependency, so the queue, watermark and mixer logic that `game` builds on is tested on Linux. `render` and `player` are Windows-only. If a `cfg(windows)` dependency leaks into the Linux build, the Windows part moves into a Windows-only crate behind the same traits.
- There is no `ui` or `skin-*` crate yet: the skin boundary sits in `render` until the declarative skin needs its own crate.
- A new portable crate goes into `REQUIRED_PORTABLE` (`xtask/src/portable.rs`) and the baseline workspace of `xtask/src/fixture.rs` in the same change. `cargo xtask portable` enforces the list; `check-arch` does not.

### Licenses

The license follows the layer. L0 through L2 (`bms-model`, `bms-parser`, `rules`, `eval`, `replay`), `eval-cli`, and the development tooling (`xtask`) are `MIT OR Apache-2.0`. L3 and above (`input`, `audio`, `game`, `render`, `skin-*`, `ui`) and `player` are `GPL-3.0-or-later`. The fuzz targets (`fuzz`, outside the workspace) are also GPL, because they link `input`; they are tooling and never shipped. The license of `ir-server` and `pkg` is decided when they are built.

- **Why split:** The game itself is copyleft so that a modified player cannot be redistributed with its source closed. Chart loading, the evaluator, and the replay format stay permissive so that other players, tools, and IR servers can load charts and verify replays with the same code. This means the judgment logic can be reused in closed products; that is accepted. The license does not lower the trademark, trade dress, or patent risk of a game modeled on a commercial one; that is handled by what goes into the code (the banned-word list, no copied assets).
- **One-way dependencies:** A permissive crate never depends on a GPL crate, for any dependency kind. The layer order already guarantees this. `cargo xtask check-arch` (R4) enforces it, and also checks each crate's `license` against an explicit crate-to-license table, so a crate missing from the table or carrying the other license fails. The workspace has no default license, so a new crate has to choose.
- **Third-party licenses:** `cargo-deny` does not allow GPL and also checks dev-dependencies, so a third-party GPL crate cannot slip into a permissive crate. It skips the workspace's own crates, whose licenses R4 checks. Every third-party license allowed today can be combined into a GPLv3 binary; MPL-2.0 code (`symphonia`, `triple_buffer`) keeps its own terms and source obligations inside it.
- **Additional permission for BASS:** Under GPLv3 section 7, the GPL part carries a permission to link with and convey the BASS family of DLLs (the alternative in "Libraries"), so that alternative stays open. Granting it now, while the GPL part has a single copyright holder, avoids collecting consent later. It does not cover third-party GPL code: the GPLv3 option of the ASIO SDK and BASS cannot be in the same binary.
- **What is not a derivative work:** Independently authored skins (including Lua skins), charts, presets, and other data the player loads, and independently authored programs that talk to it only through the local event endpoint ("Local Event Endpoint"), are separate works and not covered by the GPL. Code copied from the GPL part keeps its license.
- **Where it is written:** `COPYRIGHT` at the repository root holds the copyright notice, the license of each crate, the BASS permission, and the derivative-work statement. The license texts are `LICENSE-MIT`, `LICENSE-APACHE` (unmodified), and `LICENSE-GPL`. No per-file SPDX headers or `NOTICE` file; the `license` field of each `Cargo.toml` is the source of truth.
- **Contributions:** Accepted under the license of the crate they change (inbound = outbound), with no CLA; for a GPL crate that includes the BASS permission. Moving code from a GPL crate to a permissive one needs the consent of everyone who wrote it, so the boundary is drawn by layer up front, and code a permissive crate will need (such as the preset file loader in `game`, for `eval-cli`) moves down before others contribute to it.
- **Releases:** A `player` binary is a GPLv3 work as a whole. Its release ships the license texts, the third-party notices, and the complete Corresponding Source: the tagged commit, the vendored source of every Cargo dependency built into it (`cargo vendor`), and the source and build inputs of any native library linked into it outside Cargo, such as FFmpeg (BASS excepted by the permission above).

## Key Libraries and Algorithms

Where there is no reason to build our own, established libraries are used. We build our own only for the parts directly tied to the meaning of judgment (the evaluator, chart loading, and time computation) and the parts central to performance (the mixer). Versions and licenses are checked at adoption and managed with `cargo-deny`. Codec, compression, and measurement libraries are not put in `eval`.

### Libraries

| Purpose | Candidate | Decision |
| --- | --- | --- |
| Windows API | `windows` | Use Raw Input, WASAPI, MMCSS, and waitable timers directly. The official bindings |
| Audio output | WASAPI directly via `windows` | Used directly because low-latency shared mode (`IAudioClient3`), exclusive mode, event-driven operation, and fine-grained buffer control are needed. ASIO is an optional feature. The GPLv3 option of the ASIO SDK fits `audio`'s license ("Licenses"); the other option is Steinberg's own license, which would need its own section 7 permission |
| Audio (alternative) | BASS, BASSmix, BASSWASAPI (BASS\_FX and BASSASIO if needed) | The alternative if our own stack (WASAPI, `symphonia`, `rubato`, our own mixer) hits a dead end in terms of effort or feasibility. It has a track record in rhythm games and provides low-latency playback, scheduling at specified positions on the mixer, and pitch-preserving speed change together. It is a library whose source is not public, and its license terms differ per DLL (BASS is free for non-commercial use with no revenue; BASSASIO has a separate license), so if it is adopted, each DLL's terms are checked individually. Linking it from the GPL part is allowed by the additional permission in "Licenses". As conditions for adoption, verify by measurement that the playback position can be mapped to QPC and that clock drift can be corrected. Its latency and dropouts are measured and recorded with the same indicators, compared with our own stack where both run. The commitment not to allocate memory applies only to app-side code; inside the DLLs, it is verified by native heap measurement if needed. DLL versions, where they were obtained, and licenses are managed as dependencies outside Cargo. Audio is enclosed within the boundary of the `audio` crate, so swapping it is done within that crate |
| MIDI input | `midir` | Handles Windows MIDI input. Times are the QPC at the point the callback receives the input |
| Input timestamp measurement | GameInput (optional) | Used to record the timestamps attached to input and measure the variability of the latency until receipt (not used for judgment). Available on desktop Windows 10 1903 and later and Windows 11, but requires handling the NuGet package and the redistributable runtime, and a way to call it from Rust (our own bindings). Whether dedicated controllers are supported is checked per device, and devices that are not supported are received through Raw Input only |
| Audio decoding | `symphonia` | Handles WAV, OGG Vorbis, FLAC, and MP3 in pure Rust. It is MPL-2.0, so its obligations (publishing modified files, etc.) are honored |
| Sample rate conversion | `rubato` | Used for load-time conversion and for correcting audio clock drift (a resampler with a variable ratio). The real-time side preallocates its working buffers |
| Loudness | `ebur128` | Used for per-chart volume normalization (EBU R128) |
| Inter-thread queues | `rtrb` (single-producer single-consumer ring buffer) | One per input source, one for logic→audio, and one for logic→render (effect events). All data that must not be dropped is passed through these |
| State snapshot | `triple_buffer` | Used only for the render state snapshot (only the latest value arrives, and intermediate values are skipped). MPL-2.0 |
| Rendering | `wgpu`, `winit` | DX12 is the primary backend. All pipelines are created before play and cached |
| Text | `cosmic-text`, `glyphon` | Japanese typesetting with `cosmic-text`; GPU glyph atlas and drawing with `glyphon`. The play screen uses only atlases built in advance |
| Images | `image` | Loading PNG, JPEG, and BMP. Dimensions and decoded size are capped |
| Video (BGA) | FFmpeg (`ffmpeg-next`) | BGA formats are diverse, and decoding them ourselves is not realistic. A build without GPL or nonfree components is dynamically linked, and the LGPL obligations (providing the corresponding source and build instructions, notices, allowing the DLL to be replaced) are honored. `ffmpeg-next` is in maintenance-only status, so the option of keeping our own thin bindings stays open |
| Settings screens | `egui` | Used only for settings and management screens |
| Character encoding | `encoding_rs`, `chardetng` | BMS is mostly Shift\_JIS, with UTF-8 mixed in. Detect first, then convert |
| Chart loading | Our own (with `bms-rs` as a point of comparison) | Time handling and safe bounded loading are directly tied to the correctness of judgment, so we own it. BMSON is read with `serde_json` |
| Rational numbers | `num-rational` (`num-bigint`) | Used for time computation. Exact, with the size of the running time capped ("Chart Time") |
| Library DB | `rusqlite` (bundled SQLite, FTS5) | Song information, scores, search index |
| Fuzzing | `cargo-fuzz`, `libfuzzer-sys` (LLVM libFuzzer, NCSA) | Coverage-guided fuzzing of the decoders of untrusted input, with AddressSanitizer on a dated nightly. The targets take bytes, as the decoders do. Not used: `afl.rs` (AFL++), which needs its own toolchain and pays off in long campaigns; `honggfuzz-rs`; `bolero`, whose several engines overlap the property tests (`proptest`); typed `arbitrary` inputs, since the decoders take bytes |
| Archives | `zip`, `sevenz-rust2` | The extraction target and the size caps are always checked. RAR support is postponed because the usable decoders have license constraints |
| Settings and presets | `serde`, `toml` | Files written by people are TOML |
| Replays | Custom binary format, `zstd` | The version number goes first, time deltas are laid out as variable-length integers, and the result is compressed with zstd |
| Hashing and signing | `sha2`, `md-5`, `minisign-verify` | Identification and verification use SHA-256, and MD5 is used only for compatibility with existing difficulty tables. Signatures are unified on the minisign format (distribution registry, auto-update). No cryptography is implemented ourselves |
| Lua (for compatibility) | `mlua` (Lua 5.4) | Used only to convert beatoraja Lua skins at load time. `mlua`'s `sandbox()` is Luau-only, so for Lua 5.4 the allowlist-style environment is built ourselves |
| WASM | `wasm-bindgen` | Calls the evaluator from Workers |
| Measurement and testing | `tracy-client`, `criterion` | Span measurement, benchmarks |
| Property tests | `proptest` | A dev-dependency of `eval` and `replay`: the evaluator's properties over generated charts, presets, input logs and delivery schedules, and the input log's round trip. Generators map independent raw values to valid ones, so shrinking keeps cases valid. Not used: `quickcheck`, whose type-driven generation and value shrinking fit valid-by-construction generators poorly and which keeps no failures; `bolero`, whose engines overlap `cargo-fuzz`; `arbitrary`-driven generation, which suits the fuzz targets that take bytes |
| Errors | `thiserror` (libraries), `anyhow` (`player`, `xtask`) | `thiserror` derives the library error types and `anyhow` carries the player's and `xtask`'s reports; `eval`, `rules` and `bms-model` keep hand-written impls, since they take no external dependencies. How errors are built and printed is in "Errors" below. Not used: `snafu`, `error-stack` and `color-eyre`, which overlap these at more weight (panic hooks are out of scope); `miette`, to be reconsidered for `eval-cli` diagnostics |
| Command line | `lexopt` (`player`) | Parses the player's arguments into a `Command` before anything is opened; a usage error prints `unknown argument: <arg>` or the usage line and exits 2. Development options exist only under `dev-tools`, so a production build reports them as unknown arguments. `xtask` keeps its own small dispatcher. Not used: `clap`, whose generated error text and help would change the CLI contract the CI probe checks |
| Log file | `log` (facade); own file sink in `player` | Loading and lifecycle events and load warnings, under the data root in `logs\` ("Log File" below). Libraries return structured warnings and never log; summarizing repeated warnings happens where `player` logs them, whatever the backend. The sink is our own for its exact semantics: a byte limit checked before each write with room kept for the exit line, turning itself off on the first failed write, and a lease that lets only one running process rotate and write the files. Not used: `flexi_logger` and `log4rs`, whose size rotation runs during writing and which lack the reserved exit line, the off-on-first-error writer and the run-scoped lease; `tracing` (no spans needed; `tracy-client` is for investigations); `env_logger` (stderr by default, which a GUI build does not show, and a file sink is needed anyway); `fern`, `simplelog`, `simple-logging`, `ftail` (no size cap); `file-rotate`, `rolling-file` (writers only, without the lease or the reserve) |

Code from GPL projects (beatoraja, bmz-player, etc.) and tables ported from them are not used. Behavior is determined from public information and hand-written tests, and the sources of the information behind tables and tests are recorded.

### Scope of No Allocation

The input thread, the logic thread, the audio callback, and the per-frame work of the render thread (app-side code) do not allocate memory during play. The regions for evaluator output, the input log, and effect events are allocated in advance, and what happens on reaching the limit is decided (for the input log, it is recorded as "incomplete"). Background work such as decoding and read-ahead, and allocations inside wgpu and the GPU driver, are out of scope.

The check is done with counting global allocators in test binaries, which count only the threads that opt in. The allocation tests of `eval`, `input`, `game` (the logic step, the audio step and the audio thread) and `audio` run in CI in both the dev and the release profile with the same bound of zero, and so does the render staging test (`render --test alloc`) on Windows. An optimized build may remove an allocation whose pointer never escapes, so a release run alone can miss a temporary allocation: the dev runs stay, and each counted stretch starts with a positive control, an allocation kept through `black_box` that must be counted. `game`'s reservation tests (a play's, and an autoplay session's own buffers) measure bytes rather than zero allocations and run in dev only.

In a real play (dev-tools builds), the render thread counts its app region apart. It is a defined CPU region of each play frame in two intervals: first, draining the effects (at most what the frame's buffer holds; the rest wait for the next frame), reading the clock and the tuning, the overlay, building the `FrameInput` and `Renderer::build` (the skin's build and the staging); second, the measurement's per-frame work. Outside it are `Renderer::submit` with everything in wgpu and the driver (and the app's small computations there, such as the letterbox), the swap chain's statistics and presentation queries, waiting for the frame, the messages between plays, end-screen frames, and the frame sink (the frame dump's readback, PNG encoding and file writes, which allocate on purpose). The region's count goes to `.measure.toml` as `[allocations] render_app`, and the scripted plays assert it is 0, with the overlay shown and with a frame dump. The whole-thread counts beside it include wgpu and the end screen, and are diagnostic only.

The modules and functions that code runs also deny `unwrap`, `expect` and indexing outside tests (clippy's `unwrap_used`, `expect_used` and `indexing_slicing`); where an invariant makes one safe, an `#[expect]` states it as its reason. The list of those sites is the `IN_PLAY` table in `xtask/src/lints.rs`, which `cargo xtask check-lints` checks.

### Errors

Each error states its own step and leaves the cause to `source()`; a wrapper that adds nothing forwards the inner message (`#[error(transparent)]`). Errors that cross a thread or are compared in tests keep I/O failures as `game::IoFailure`, the kind and the OS message. A report prints the whole chain once, joined with `: `: the player's top level prints `{:#}` of an `anyhow::Error`, which only the setup, loading, persistence and shutdown paths build, and the in-play and between-play prints (a lost frame, an unsaved record) use `game::Chain`, which formats the same line without allocating. The top-level report line is also written to the log ("Log File"). Usage errors (`player`'s `args::UsageError`, found before any file, device or log is opened) exit 2 and runtime errors exit 1. The `IN_PLAY` files never name `anyhow`, so in-play code keeps typed errors; `cargo xtask check-lints` checks it.

### Log File

`player` writes a log for the commands that do work (`play`, `keys` and, in development builds, `measure play`): the run's version, build and command, each loaded chart with its warnings, the sound bank's warnings, the failure line, and the exit code. What the commands print stays on stdout and stderr as it is; the log is a copy, and a failed open adds one `log:` line on stderr.

- The logger starts after argument parsing, so usage errors keep exit 2 and their text and write nothing; `--version` and the development probe write nothing either.
- It lives in `logs\` under the data root. Each run truncates `tapestry.log`, and the two runs before it are kept as `tapestry.1.log` and `tapestry.2.log`, rotated once when the run starts. A run first takes a lease on `logs\tapestry.lock` (an exclusive file lock held for the life of the process, never removed): a second process that finds it held neither rotates nor writes.
- Bounds: 1 MiB per file, with 256 bytes kept for the exit line, which says how many records the cap dropped; 1 KiB per line, with line breaks replaced so one record is one line; 100 warnings per source per load, then a count of the rest.
- A chart's missing sound files are one line, not one per `#WAV`: the count and the first three names, with control characters escaped, on stderr and in the log.
- Best effort: a failed open prints one `log:` line on stderr and leaves logging off; a failed write turns it off silently. Neither changes the outcome or the exit code. A missing exit line means the run ended without reaching it, or that logging failed.
- Each record is written to the file before the call returns, so nothing logged is lost on a panic or an early exit; there is no panic hook.
- Only `tapestry` targets at Info and above are written. Every other crate's record, including wgpu's on the render thread and symphonia's in the decode pool, is rejected before the sink's lock and is never formatted. The arguments of those crates' own log calls are still evaluated under the global Info level, which is inside their code, not the app's.
- The `IN_PLAY` files never name the `log` facade, so in-play code does not log; `cargo xtask check-lints` checks it.

### Algorithms

| Process | Method |
| --- | --- |
| Chart time | Measure positions are accumulated into time as exact rationals, segment by segment between BPM changes and STOPs, with the size of the running time capped, and rounded to µs once ("Chart Time") |
| Scroll position | Note heads, long-note ends and bar lines store their display positions, computed at load time from exact beat positions, and a piecewise linear map from integer µs to display position is built from the exact timeline: a segment per BPM change, STOP start and STOP end, starting at the first whole µs of its exact time and anchored on the exact curve ("Chart Time"). The renderer reads the map only for the playhead; per lane it finds the visible notes by binary search on the stored positions, which never decrease. Scroll and speed change channels are ignored with a warning, which changes the look but not the judgment. Reverse scrolling and notes overtaking each other are later variations that change chart loading and rendering: a position becomes the integral of the scroll factor over beats, still computed at load time, the map takes signed slopes, a note index sorted by display position finds the visible notes at any slope with a bound on the notes it visits, and LN bodies are laid out without assuming the end is above the head. Evaluation stays in time order and does not change |
| Hi-speed | The setting is the green number (the time a note is visible), 100–4000 ms, starting at 600 ms (tuned by playing). Each step changes the scroll speed by the same ratio, 6^(1/34) ≈ 5.4 %, through `round(100 × 6^(k/34))` ms for k = 0 to 70, a ladder that holds both bounds and 600 ms; a fixed step in ms would be 4 % a press at 600 ms but 17 % at 150 ms. The scale is fixed at play start from a base BPM, for now the exact start BPM (the scroll map's initial rate) (another base comes when a player asks for it), so the visible time is exact at that BPM. Notes then move through the scroll positions at that fixed scale, so BPM changes and STOPs show as the chart intends and nothing jumps. Scaling by the BPM in effect (a constant green number) is not used, because the notes jump at every BPM change, and neither is placing notes by time alone, which erases the chart's speed changes. Lane cover and lift positions are included in the visible range calculation |
| Evaluation event processing | Per-lane deadline arrays, input-dependent deadlines (scratch release; later debounce and HCN) and the merged input stream are taken out in one fixed total order: by time, inputs before deadlines at equal times (a preset item), inputs in the merged stream's order (source index, then the order within the source), deadlines by kind, then lane, then sequence number ("Event Order") |
| Note assignment | Each lane keeps a position pointing at its first unprocessed note, and only notes within the judgment window are candidates. How to choose among candidates is decided by a preset choice; the custom preset takes the nearest unjudged note inside BAD, ties to the earlier one, so a late press moves on once it is past the midpoint between notes (a press exactly at the midpoint keeps the earlier note). What the judgment does to earlier unjudged notes is the miss choice: the custom preset misses them at the press, so notes are never judged out of order. Several notes of one lane at one µs follow the same rule, one assigned hit per press: presses at that µs take them one each, the earliest in position order first, and a press that judges a later note misses them at once (a press at 101 µs judges the note there and misses both notes at 100 µs). Empty POOR looks only at taps and LN heads: mines are never its targets |
| Debounce | Handled by a small state machine per button. Changes that are too short are dropped, but so that dropping a release does not leave a key held down, the final state always returns to the actual state. When multiple buttons are assigned to one lane, the lane is pressed if any of them is pressed |
| Analog scratch | Axis values are converted into a continuous position, taking wraparound into account, and changes in position are accumulated (for devices that report deltas, the deltas are used as is and not accumulated twice). Separate thresholds for press and release (hysteresis) are used, and a reversal of the rotation direction is treated as a new press. The release at a fixed time after movement stops is handled as an evaluator deadline |
| Gauge | Fixed point: an `i64` in units of 1/100000 %, intermediate computation in `i128`, the order fixed as compute the change → clamp to the bounds, remainders floored ("Gauges and Session Policy") |
| Audio clock correction | Sound starts are scheduled through the frame-to-QPC mapping re-anchored on every period (the "Aligning Audio to the Judgment Timeline" subsection), so drift shows only inside a long running sound, and the measured drift is recorded. When that drift calls for it, a variable-ratio resampler is added: the difference between the device position and the position derived from QPC is measured at regular intervals, smoothed to remove noise, and the conversion ratio is nudged slightly with proportional-integral control. Integral windup is prevented, the ratio and its rate of change are capped, and the controller is reset on discontinuities such as pause and resume. Conversion latency is counted in the press-to-sound figure |
| Mixing | Preallocated voice slots are reused. A start first stops every voice and fading voice of its own #WAV id, without a fade (the earlier instance's last sample is the one before the new start's first, so starts at the same sample sound once); different ids on one stored file are not affected. If it stopped a playing voice, it takes that voice's slot; stopping only a fading voice frees no voice slot. Otherwise it takes a free slot, and when every slot is busy the oldest voice is stolen and fades out over 2 ms, a voice that has not sounded yet being dropped instead. Sounds held as i16 (to save memory) are summed in f32, scaled by the master volume and a fixed headroom of 0.5 (-6 dB, so a full-scale background and a full-scale key sound together just reach full scale), passed through a static soft clipper that is the identity up to a knee of 0.8 and saturates smoothly (tanh) to full scale above it, and converted to the output format. The mix never rides its gain with the program: a peak limiter would lower the whole mix whenever key sounds push the sum over full scale, so the background would audibly dip and pump back with play. The clipper has no state, look-ahead or latency, and rendering BMS charts with every key sound at its note time puts well under 1 % of samples above the knee |
| Per-chart volume normalization | The sound of playing all key sounds and the BGM with autoplay is analyzed in the background with EBU R128 (gated loudness and true peak) and cached, and from the next play it is applied as a fixed correction. The first load does not wait for it |
| Recommended offset | Computed from press deviations only: a long note's judgment carries its head's deviation, and a release adds none. Only when enough deviations have been collected, the median and the spread are output. This value includes not only device latency but also the player's habits, so it is presented as "a recommendation based on timing bias" |
| Rendering | Sprites are grouped per texture and drawn at once with instanced rendering, as far as layer order and translucent overlap can be preserved |
| Song search | In addition to Unicode normalization (full-width and half-width), our own conversion that removes the distinction between hiragana and katakana is applied before indexing. Substring matching uses the FTS5 trigram tokenizer, and short queries of 2 characters or fewer are supplemented by another method (prefix matching, etc.) |
| Library scanning | Only files whose modification time or size has changed are selected, and only their headers are read. Processed in parallel on multiple threads |
| Chart storage (distribution) | Files are stored by the SHA-256 of their contents, and identical files are not stored twice |

## Testing Strategy

Verify the properties the evaluator must satisfy, not agreement with the results of existing implementations.

- The same input log and the same preset always give the same result (even across different builds and environments)
- The result of incremental evaluation during play matches the result of batch evaluation
- With the order of inputs preserved, changing the steps by which the watermark advances does not change the result
- In single-note judgment with a fixed mapping between input and note, widening the judgment window never makes the judgment worse (in whole-chart evaluation this does not hold, because an earlier input can take a different note)
- Survival gauges never recover once they reach 0
- Gauges stay within their bounds
- The order of inputs at the same time does not depend on how the sources' events were recorded or staged
- Given the same section, the same input conversion state at the start of the lead-in, the same lead-in input, the same time mapping, and the same offset changes, the evaluation of a section excerpted from a full input log matches the evaluation of a mid-chart start

Whole-chart evaluation focuses on the fragile spots: inputs exactly on a judgment window boundary, the processing order of events at the same time, mashing, LN release and re-press, and inputs that arrive exactly at the watermark boundary.

From the IR stage on, agreement between the native and WASM builds is verified end to end, not just for the evaluator. From the chart file's bytes and the input log, the resolved chart hash and the judgment event stream are computed on both the native build and the actually deployed Workers, and compared. The loader, evaluator, and preset versions are fixed, and overflow handling and the order of events at the same time follow explicit rules from stage 1. Agreement between incremental and batch evaluation is also verified with input logs that include offset changes and events at the same time.

In addition, each preset has a few dozen small hand-written examples of "this input gives this judgment", to detect unintended changes when presets are tuned. Hand-written examples stay alongside the property tests, because live and batch evaluation share code and can share a bug.

The properties above are property tests over generated charts, presets, input logs and delivery schedules, except the section excerpt, which comes with practice (stage 2). Live evaluation in them is a model of the logic thread's consumer ("Watermark") over bounded producer queues. A deterministic test checks that the generators still reach the fragile spots, including joint ones such as an input on a deadline at an intermediate watermark, which random generation alone rarely reaches. Seeds and failures are handled as in [setup.md "Property Tests"](setup.md#property-tests).

### Verifying Without Launching the UI

So that each check does not require launching the app and operating it by hand, the whole game is built to run without a screen (the testing mechanisms in the "Overall Architecture" section).

| Target | Method | What it verifies |
| --- | --- | --- |
| Evaluator, chart loading, presets | Unit tests, property tests, hand-written examples, `eval-cli` | Judgment, gauge, time computation |
| Chart loading, input log decoding, Raw Input packet decoding | Fuzzing with `cargo-fuzz`: short runs on pull requests, long runs daily | No panic, timeout, or out-of-memory on broken input |
| Whole game (headless) | Run one song through at maximum speed, with a virtual clock and a fed input log | Screen transitions, judgment events, sound scheduling positions, retry, practice sections |
| Audio | Generate waveforms offscreen with the same processing as production, with the input PCM, sample rate, initial state, and block length fixed. The sample positions where sounds were scheduled are compared exactly, and waveform values within a tolerance, against the expected output. Including flushing the tail, also verify that changing how the blocks are divided does not change the result | Mixing, soft clipper, volume normalization |
| Rendering | Render the screen at a given time offscreen and compare it with the expected image within a tolerance. On Windows CI, the DX12 adapter that renders in software (WARP) is explicitly selected, and the adapter used is recorded. If it cannot be obtained, the test fails rather than being silently skipped. WARP results can also vary by version, so the Windows image, wgpu and the shader translator, fonts, and rendering settings are pinned, and the diff images are reviewed on updates. Presentation mode, VRR, and display latency cannot be verified this way | Skins, note positions, effects |
| Performance | Benchmarks of fixed scenes. CPU side on CI, measurements including the GPU on the development PC | Regressions in the indicators |
| Allocation | Counting global allocators in test binaries, run in the dev and release profiles with a positive control each, and the render thread's app region count in scripted plays (the "Scope of No Allocation" subsection) | No allocation during play |

Test charts are made with a small mechanism that builds charts in test code and with small self-made BMS text files (to exercise the loading path), and copyrighted charts are not put in the repository. Checks with real charts are done in local environments.

Chart loading has two kinds of tests. Preservation tests pin what judgment sees: every fixture's evaluator-visible projection (lane order, note and end times, kinds, mine damage, sounds, rank, TOTAL, LN mode, BGM, ordered warnings, and whether it loads) and the evaluation event stream for a frozen input and preset, against recorded digests; a change to display geometry alone must leave them passing. Accuracy tests check display geometry against exact arithmetic done independently in the test, never against the scroll map under test, and each rejection test has a matching accepted case.

Tests without threads do not find bugs in notifications or queues. So a small number of tests also run on real threads, to verify input order, the watermark, and stop and resume.

### Shortening Launch-Based Checks (Development Builds Only)

- **Scenario launch**: The app can be launched with a scenario (`--scenario scenario.toml`) that describes "load this chart, feed this input log, save the screen at this time, exit". The same scenario is used whether a human or an AI does the checking.
- **Specifying the start position**: From the command line or a scenario, the app can be launched with a specified start position for the practice "mid-chart start" (the "Input Logs and Replays" section). It uses the same mechanism as the product's practice feature, and the development build adds only the way to specify it. It is used in combination with preset and skin reloading.
- **External control** (once the local event endpoint exists): Load, start, and screen-save operations are added to the local event endpoint. The event endpoint is by nature receive-only, so this is an exception for development builds only, and the connection token and loopback restrictions still apply.
- **Measurement output**: On each judgment, the app can flash a square at a fixed position on the screen and emit a test sound (observable with a high-speed camera, or with the optional test device below).

### Separating Development and Production Builds

- All of the features above go only into builds with a Cargo feature (for example `dev-tools`) enabled. Rather than hiding them behind a runtime flag, the production build does not contain the code itself.
- The production build is made with `dev-tools` disabled. Features can be enabled even with the `release` profile, so the distribution CI pins the feature configuration. Development command-line arguments (`--scenario`, etc.) do not exist in the production build, and are treated as unknown arguments if passed.
- CI builds the production build and verifies that it does not accept development arguments and that the event endpoint has no development operations.
- Replays and scores made with a development build record that they come from a development build, and cannot be sent to IR.

### Test Input Device

A test input device that an AI can operate is an optional benchmark the maintainer may build and run; it is never part of the required workflow. No test, CI job, exit criterion or contribution step needs it or any other special hardware, because that would make contributing depend on owning the device. The required checks rely on the game's own figures ("Latency Figures") and the human checks below; the device only adds absolute end-to-end numbers on top of them. Its design, if built, is described in a separate document; here only its role is defined.

- **Reproducing input**: A microcontroller (such as a Raspberry Pi Pico 2) connects to the PC as a USB HID device (keyboard or gamepad, 1000 Hz) and presses according to a scenario received in advance over a control serial link. Presses and releases are sent one at a time, in order, as reports, and the state is never overwritten by the next change before its report has been sent (so that short presses are not lost). The "press time" is defined as the value of the microcontroller's µs timer at the point that report is handed to USB transmission. 1000 Hz is the report interval, not a guarantee of delivery at µs precision.
- **Estimating latency until receipt**: The microcontroller and PC clocks differ in both origin and rate, so the press time and the receive time cannot be compared directly. Presses and releases carry sequence numbers to match them with the received events, and the mapping between the two clocks is calibrated repeatedly with round trips over the control serial link. Round-trip calibration cannot determine the one-way latency, so the results are treated as estimates with error bounds, and used to observe the spread of the latency distribution and its changes.
- **Measuring actual latency**: A light sensor attached to the screen detects the measurement square, and capturing the audio output detects the test sound. Press-to-light and press-to-sound are measured with the single clock of the microcontroller, so no calibration between clocks is needed. The audio capture includes bias, attenuation, and input protection circuits, and the detection threshold and which sample's time counts as the detection time are decided in advance.
- **A mechanism that presses a real controller** (solenoids and motors) is considered only if the two above prove insufficient.

To the PC, this device looks like an ordinary HID device, so the game needs no dedicated input path. Also, it does not reproduce the quirks of real dedicated controllers (polling jitter, the scratch detection method), so it does not replace checks with local controllers.

### What Humans Verify

Feel, local dedicated controllers, running alongside OBS (once that variation is supported), behavior when the window loses focus, device disconnection and reconnection, and recovery after changing the audio device are compiled into a per-release checklist and verified by humans.

## Roadmap

| Stage | Contents | Exit criteria |
| --- | --- | --- |
| 0 | A short technical spike (meant to be thrown away) on the development PC at WQHD 240 Hz: run high-speed scrolling with 10,000+ notes, 100+ simultaneous voices, 1080p BGA, and effects on all lanes at the same time. When done, move to stage 1 immediately | The rendering and audio approach runs on the real path on the development PC. Each player-visible problem found has a cause and a design response, or is recorded as an open question or a later variation. The indicators the spike can measure are implemented, and their stage 0 values are recorded as a baseline; the others are recorded as not yet measured |
| 1 | The minimal setup for playing one song: chart loading, input, audio, a simple play screen, an evaluator with one custom preset, saving the input log. Also includes key config, volume, offset, hi-speed, and instant retry. The testing mechanisms (swappable clock and I/O, processing that can advance without threads) are also put in here | One chart on hand can be played to the end without feeling latency or stutter. Key sound and frame latency can be measured and displayed |
| 2 | One full gameplay loop: song selection, results, retry, practice (section repeat and speed change), beatoraja-style and LR2-style presets with switching on the result screen, `eval-cli`, and development features such as scenario launch. | Song selection through play, results, and retry can be played repeatedly. The property tests pass |
| 3 | Look and effects: the declarative custom skin format and polished effects. The LR2 and beatoraja skin adapters come after this, as variations ("Extension Boundaries") | Playing with the standard skin is fun |
| 4 | Custom IR (server-side computation, start registration, impossible-input detection, allowlist) | Input logs can be sent, and rankings for multiple presets can be viewed |
| 5 | Chart registry (catalog only) and installer | Difficulty tables can be installed in one go, and derivative chart dependencies can be resolved |
| 6 | Connecting to existing IRs | Scores computed with the specified preset can be sent in the existing format |

> **Drawing the line on scope**
>
> Until stage 3 is finished, IR, the distribution registry, anti-cheat, and servers are not touched. The only things decided before then in preparation for a future IR and the like are the input log format and the evaluator boundary, so that these can be added later. The later sections of this design document are designs for building at that point, not reasons to build earlier.

Chart features beyond what a stage shows wait for the stage that shows them ("What the Loader Reads").

After stage 3, variations (other chart formats, input adapters, skin adapters, settings profiles, other PCs, running alongside other software) are added one at a time as needed, before or alongside the later stages. Between stages 1 and 2, a generous period of playing and fixing is set aside. IR (stage 4 onward) is started once a state that is fun to play exists. The input log format carries a version number from stage 1, and only its core parts (physical input, time, chart identification, and the preset record) are included from the start. Other fields are added while bumping the version. Crates, too, start out consolidated into a few, and are split once the boundaries become clear.

## Open Questions

- In the IR rankings, when presets or the evaluator change, whether to create new rankings or recompute and replace them
- When replays are published, how much of the input log to show to others
- How much expressiveness (computation, conditions, changes over time) the custom skin expressions should cover
- How far LR2 skin support should go
- In chart distribution, how to record authors' distribution terms and how to obtain consent for hosting (requires consultation with event organizers and difficulty table maintainers)
- The criteria for inclusion in the allowlist, and the takedown request procedure (including legal judgment)
- How to obtain authors' permission for "storage for evaluation", and the form of that record (to be decided before starting IR)
- How the game keeps a discrete GPU out of low performance states under the driver's default settings ("GPU Power State")
- How to watch frame pacing under VRR, where counting in whole refreshes does not apply ("Frame Pacing in Refreshes")

## Appendix: Lessons from Existing Implementations

Points read from the source code of beatoraja (Java) and Lunatic Vibes F (C++, an LR2 clone) that are reflected in this design.

| Item | Existing implementations | Treatment in this design |
| --- | --- | --- |
| Input recording | beatoraja keeps only the latest state per key, so one half of a short press and release can be lost | Append every edge |
| Judgment timing | beatoraja has a judgment thread, but its time advances only with render frames. LVF uses 1 ms steps, and can finalize a miss before processing input | Time-ordered event processing and the watermark |
| Clock | LVF handles `system_clock` in ms | QPC and integer µs |
| Coupling of judgment and other processing | Both directly modify key sounds, skin timers, settings, and chart state inside judgment | Emit only the judgment event stream |
| Rule switching | beatoraja has rule code for LR2, but it is never selected. LVF has the LR2 rules fixed | Presets made of data and choices |
| Replays | Neither saves the judgment rank or the version. beatoraja's playback results fluctuate depending on frames | Record every input needed for evaluation |
| Gauge | Both compute all gauges side by side, but in floating point. beatoraja records the history every 0.5 seconds | Fixed point, recorded per judgment event |
| Offsets | Both have only a display offset, and their automatic adjustment rewrites the settings during play | Three offsets. Suggestion of a recommended value, and in-play adjustment that is recorded and verifiable |
| Skins | beatoraja uses 968 ID constants, and LVF uses global arrays directly as LR2 IDs | The state snapshot, and conversion inside adapters |
| Tests | beatoraja has none, and LVF has one judgment test | Property tests and hand-written examples |
