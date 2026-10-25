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
| Input | Receives device events, attaches QPC times, and appends them to the input log. Publishes the watermark | Judgment, processing that allocates memory |
| Logic | Wakes on input publication and watermark updates, and advances the evaluator up to the watermark. Passes judgment events to audio and rendering | Waiting on rendering or file I/O |
| Audio | Mixes in the WASAPI callback. Plays key sounds and BGM | Acquiring locks, allocating memory |
| Render | Reads the state snapshot and draws the skin | Writing to the evaluator or the chart |
| Loading | Loading charts, audio, and BGA, and decoding video | Heavy processing during play (finish it beforehand) |

Threads are connected by lock-free single-producer single-consumer queues. One queue is placed per input source, one for logic→audio, and one for logic→render (effect events), and all data that must not be dropped is passed through queues. Triple buffering is used only for the render state snapshot, where only the latest value is needed. There is no shared mutable state.

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

## Input

- **Receiving**: A dedicated thread receives Raw Input (keyboards, HID dedicated controllers). MIDI is received in a separate callback and normalized to the same format. How times are attached follows the "Time Base (QPC)" section. Even when GameInput is also used, the same device is never ingested twice through two paths.
- **Keyboard**: Treated as the standard device that people without a dedicated controller use first. Input is recorded by physical key ID, so a standard key layout, scratch assignment, and a screen for checking simultaneous presses (to notice the keyboard's simultaneous-press limits) can be added later within the input layer.
- **Time**: A QPC time is attached at the moment the event is received (the receive time). It is the true receive time: the floor of the QPC read taken when the batch was received, the same for every event of that batch. The constant delay until the OS delivers it is corrected by the input latency offset.
- **Physical input is the source of truth**: What is recorded is "which button or axis of which device changed, when, and how". The mapping to lanes (key config) is recorded, and the evaluator applies it. This also reproduces multiple keys assigned to one lane and per-device debounce.
- **Debounce**: Not applied at the recording step. The evaluator applies it later as a device setting.
- **Sources**: One physical device is one source. It owns one queue, one input latency offset, and all its controls, buttons and axes alike; several keyboards are several sources, even when they share a key binding. Sources get anonymous session-local ids (0, 1, … in the order devices are found when recording opens); interface paths, names and serials never enter the input log. Source bookkeeping is preallocated with a fixed maximum, and a device beyond it does not take part; the input log is then marked "incomplete".
- **Participants are frozen**: The set of participating sources is fixed when play recording opens and does not grow during the play. A device that arrives during play is lifecycle metadata only: its arrival time and kind go into a bounded arrivals list outside the input stream and the watermark, and it takes part from the next play.
- **What is recorded**: Only controls bound in the key config (lanes and system controls), and only while play recording is open. Unbound controls cannot affect evaluation, so an input log never holds typed text. While recording is closed, the key config screen may capture any control to bind it; nothing captured there is recorded.
- **Held-input snapshot**: The input thread keeps each device's physical state (pressed controls, last axis values) for the whole run of the game, across retries. A control of a device not seen since the game started counts as released. Recording opens at a boundary between two batches: the held state of the participating sources is copied into the start snapshot of the play, and every event pushed after that point, in stream order, belongs to the play. Input conversion starts a held control as pressed with no press edge, so nothing is judged from it, and its later release is a plain release. A release of a control that is not held changes nothing.
- **Removal**: Removing a participating device records a disconnect event for its source (see "Analog Scratch" for what input conversion does with it). The source publishes the removal time as its final watermark and is closed. It stays in the watermark minimum until all its queued and staged events are merged, and is retired only then, so a removed device neither stalls the watermark nor loses an event. Removing the last participating device aborts the play.
- **Focus loss**: Losing focus during play aborts the play. The window's focus-loss notification sets a latch that the input thread checks before each batch, and any Raw Input packet received while the game was in the background is rejected before it can be recorded and trips the latch too. Once the latch is set, nothing more is appended; the play ends at the last finalized watermark and is saved as "ended midway".

### Analog Scratch

Axis values are recorded raw with timestamps, and the evaluator converts them into scratch presses and releases based on the device settings. The settings include the following, and all of them are recorded in the replay.

- Whether the value is absolute or relative, wraparound on a full revolution, inversion of the rotation direction
- The rotation threshold for counting as a press, and the time after movement stops before counting as a release
- The rule for combining button input and analog input on the same lane
- When a device is disconnected, a disconnect event is recorded in the physical input stream. On it, input conversion releases every input of that device that was pressed and clears that device's pending debounce and scratch state, so live play and re-evaluation handle it identically

### Watermark

The watermark is the boundary that "no input with an earlier evaluation time will arrive from now on". The evaluation time is the receive time minus the input latency, converted to the chart's timeline.

- Each input device is its own source, with its own dedicated queue and its own input latency offset (devices sharing one receive path, such as several Raw Input devices, are still separate sources). A source puts all the events of a batch into its queue, then reads QPC afresh and publishes `w_i = floor(fresh QPC) − 1 µs`. This order is kept. Any later batch takes its own QPC read after that, so it is stamped at or above the fresh value and never at or below a published `w_i`; receive times stay true. Within one source, events keep their receive order, and because one offset applies to the whole source, that is also their evaluation-time order.
- The overall watermark is the minimum, over the participating sources not yet retired (see "Removal" under "Input"), after converting each source's receive time into an evaluation time: `min_i(w_i − offset_i(w_i))`. When every source is retired there is no watermark, which only happens when the play is aborted. The logic thread merges the events of the queues into one stream ordered by evaluation time, with a fixed tie-break (source index, then the order within the source), before passing them to the evaluator.
- For sources that deliver nothing on their own, such as a MIDI device with no input, the input thread periodically publishes on their behalf a notice that "there is nothing up to this time" (a receive-time update), so the watermark does not stall.
- Events with the same time as the watermark are guaranteed to have been published before the watermark is published.
- The input thread wakes at least at 1 kHz and advances the watermark even when there are no new events.

> **Operations unrelated to judgment go in a separate stream**
>
> Hi-speed and lane cover changes are recorded as a stream separate from the judgment input. The evaluator reads only the judgment stream.

## Evaluator

The evaluator is a discrete-event processing system that puts input events and note deadlines (miss for an unhit note, LN release, HCN gauge increase and decrease, mines, song end) in a single queue and processes them in time order. It depends neither on frames nor on a fixed time step.

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

/// Batch evaluation used for results, replays, and IR. Internally push_input and advance_to(end)
pub fn evaluate(chart: &Chart, log: &InputLog, rule: &RulePreset, range: Option<&RangeSpec>) -> Evaluation;
// RangeSpec: section, lead-in length, initial gauge value (section evaluation in the "Input Logs and Replays" section)
```

### Finalize Only Up to the Watermark

During play, only `advance_to(watermark)` is called. Even for a note whose deadline has passed, the miss is not finalized until the watermark passes that deadline. Therefore, a miss is structurally never decided ahead of an input that was still on its way. The finalization delay is determined by the watermark interval (about 1 ms with the input thread's 1 kHz wake-up). It cannot be guaranteed on Windows, so it is measured and tracked.

### What Wakes the Logic Thread

- It wakes both on input event publication and on watermark updates. Even without input, misses, HCN, mines, and song end are processed on watermark updates.
- Even if a wake notification is missed, advancing to the latest watermark on the next wake gives the same result.
- If an input queue overflows, that play's input log is recorded as "incomplete" and excluded from replays and IR. Input is never silently dropped.

### The Only Output Is the Judgment Event Stream

- Judgments (note, judgment type, deviation in µs), combo changes, gauge changes, key sounds to play, and LN state changes are emitted as timestamped events. Events carry sequence numbers.
- Key sounds, bomb effects, and score display are the job of whoever receives the events. The evaluator touches neither audio nor skins.
- Which note's sound plays on a press also varies by rule, so the evaluator emits the "sound to play" as an event.

### Determinism

- Time is integer µs, and the gauge is fixed-point (the rounding rules are fixed by the evaluator version). No floating point is used.
- The processing order of events at the same time is made explicit as a preset item.
- Anything that uses randomness (`#RANDOM` branches, RANDOM-type options) is decided at the step that resolves the chart before evaluation, and the result is recorded. The evaluator itself uses no randomness.
- Even for the same chart file, a change in the loader version can change its interpretation. So the loader version and the hash of the resolved chart are recorded.

## Rule Presets

Rule differences are expressed in three ways: data (numbers in TOML), fixed choices (`enum`), and rule-specific implementations built into the evaluator. All of them are code inside the evaluator, not plugins that users load later. A preset holds the data and choices plus the name and version of the evaluation implementation to use, and can be serialized in full and hashed. This is so that which rule was used for evaluation is reliably recorded.

| Level | Contents |
| --- | --- |
| Data | Judgment windows (per judgment, early and late sides separately; separate tables for normal notes, scratch, and LN ends)<br>Conversion table from `#RANK` / `#DEFEXRANK` to judgment window multipliers, default value when `#TOTAL` is absent<br>Empty POOR range<br>Gauge (initial value, lower limit, clear threshold, per-judgment increase and decrease, reduction at low remaining gauge)<br>Score weights (EX score), what counts toward BP, processing order of events at the same time |
| Choices | How a press is assigned: nearest note / oldest unprocessed note / combo priority<br>LN judgment mode: LN / CN / HCN (the default when the chart does not specify one, and whether it overrides the chart's specification)<br>How `#TOTAL` applies: multiplies the increase / also corrects damage<br>Mine condition: held while it passes / pressed at its time |
| Fixed | Judge by input time, process in time order, survival gauges never recover once they reach 0, time is integer µs |

> **LN "notation" and "judgment mode" are different things**
>
> `#LNTYPE` and `#LNOBJ` express how LNs are written in the chart. The loader interprets them and reconstructs the LN spans (start and end). Whether to judge them as LN, CN, or HCN is decided separately. If the chart specifies a judgment mode (such as beatoraja's `#LNMODE`), that value is used; otherwise the preset's default is used. Only when the preset is set to "override the chart's specification" is the preset's value used.

### Standard Presets

- **Custom (default)**: Judgment specific to this player. Its values differ from the judgment of the commercial game this is based on.
- **beatoraja-style**, **LR2-style**: Aim for similar behavior based on public information. They are named "-style", not "compatible", and an exact match is not promised.

Items are added when a preset that needs them appears. Unused tuning items are not built in advance.

### Handling Differences Not Expressible as Numbers

What is kept is not "express everything with data only", but the contract as seen from outside the evaluator: the process that produces the judgment event stream from `(chart, input log, preset)` is pure and deterministic, and is recorded together with versions. Within this contract, support is extended in the following order.

1. **Differences expressible as numbers**: Handled by preset data.
2. **Differences not expressible as numbers**: Add one choice to the relevant axis (e.g. the condition for missing an unhit note, whether an empty POOR breaks the combo). Rule differences are gathered into one module per axis, so that adding a choice stays within that axis. Do not scatter code that branches on the "whole rule" across the judgment code.
3. **Too different to split into axes**: Add one evaluation implementation dedicated to that rule inside the evaluator. Presets select it by name and version, and the contract seen from outside does not change.

There is no need to exhaust this order. If several behaviors are intertwined and splitting them into independent axes would be unnatural, a dedicated implementation from the start is fine. Only combinations of axis choices whose behavior has actually been verified are allowed. Start with one evaluation implementation and concrete examples, and add axes or implementations only when an actual rule demands them.

Differences are found in the process of writing hand-written examples per preset ("this input gives this judgment"), and each time one is found, an axis or a choice is added. For a change that alters meaning, the evaluator version is bumped and the results are not mixed with past records.

## Gauges and Session Policy

Gauges are divided into two kinds by how a clear is decided.

| Kind | Examples | Clear condition | Failure |
| --- | --- | --- | --- |
| Decided by the amount remaining at the end | ASSIST EASY, EASY, NORMAL | The amount remaining at song end is at or above the clear threshold | Never fails midway |
| Decided by survival | HARD, EX HARD, dan-course gauges, etc. | Stays above 0 through the end of the song | Finalized the moment it reaches 0 and never recovers afterward (recorded in `failed_at`) |

- All gauge types included in the preset are computed side by side.
- For survival gauges, separately from the real value (which stops at 0 on failure), "the trajectory had it not failed" is computed in separate state as a reference value. The reference value is not used for clear determination.
- Gauge history is recorded for every judgment event. It is not thinned out at fixed intervals.

### Session Policy Lives Outside the Evaluator

The following three are not rules but policies for how play proceeds, and they are kept outside the evaluator. The evaluator only computes all the gauges; it does not decide which one to display or when to stop.

- **Choosing the displayed gauge**: fixed / automatically switch to the highest surviving gauge (GAS)
- **Handling failure**: stop there / keep playing to the end
- **Automatic offset adjustment**: disabled / enabled (adjustment rules). See the "Time and Offsets" section

> **What recomputed results mean**
>
> A result recomputed under another rule is the value of "what if the same input were judged under that rule", not "what would have happened had you been playing under that rule". Players change how they press based on the judgment display on screen, and once play stops on failure there is no input after that. The screen shows this difference in its wording, and the part after the stop is shown as "no input". With the policy of playing to the end, the input log is always complete through the end.

## Input Logs and Replays

A replay holds the inputs needed to reproduce evaluation. Judgment results are kept only as a cache and are not treated as the correct values. The format carries a version number, and fields can be added by bumping the version.

| Field | Contents |
| --- | --- |
| Versions | Replay format version, evaluator version, loader version, app version |
| Chart | SHA-256 and MD5 of the chart file (MD5 for compatibility with existing difficulty tables and IRs), hash of the resolved chart |
| Chart resolution | `#RANDOM` branch results, options (RANDOM, MIRROR, etc.) and seed, the resolved lane mapping |
| Rule | Preset ID, content hash, a copy of the preset itself. Session policy |
| Device | Device type, receive method (Raw Input, MIDI, etc.), the record of report intervals and the estimated polling rate (or "not estimable"), key config, debounce, analog scratch settings, the three offsets |
| Play type and evaluation conditions | Play type (full, mid-chart start, section repeat), section, attempt number, lead-in length, speed (the mapping between real time and chart time), initial gauge value (see the "Practice Records" subsection) |
| Physical input | A sequence of `(pre-evaluation receive time µs, source ID, button/axis ID, value)` with the disconnect event of a source in the same stream. The held-input snapshot (`InputStart`: the time recording opened and the controls held at that point) precedes it. Devices that arrive during play are a separate arrivals list (time and kind), outside the input stream. Times are delta-encoded per source and compressed |
| Offset changes | Changes applied by automatic offset adjustment (time, new value) |
| Other operations | Hi-speed and lane cover changes (a stream separate from the judgment input) |
| Status | End status (played to the end, ended midway), the finalized watermark at the end (an aborted play is re-evaluated up to it), whether input is incomplete and why. Start token for IR (optional) |

The format uses a custom schema, and version detection does not rely on the hash of the serialized output. For a chart of about 2000 notes, it fits in a few tens of KB even before compression.

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

`evaluate` accepts these section conditions (section, lead-in length, initial gauge value, offset changes). Without the conditions, it evaluates the whole chart.

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

## Audio

- The standard output is low-latency event-driven WASAPI shared mode. The supported period range (integer multiples of the fundamental period) is queried with `IAudioClient3`, and the shortest period that runs stably is chosen. If initialization with a short period fails, it falls back to normal shared mode. In shared mode, OBS and the like can capture desktop audio and other apps' audio as is.
- Exclusive mode is an optional setting for those who want to cut latency further. The settings screen states that "other apps' audio may stop playing on that output device, and the streaming software's normal desktop audio capture cannot capture the game's audio". ASIO is also an optional setting, and the screen states that whether it can coexist with other software depends on the device and the audio routing.
- The audio thread waits on the WASAPI event handle and writes the next buffer each time it wakes.
- The mixer is our own. Only the key sounds referenced by the chart are decoded to PCM before play starts, converted to the device's sample rate, and kept in memory as i16. Files with identical contents are merged into one.
- Every sound, including a whole song's BGM in a single file, is decoded and kept in memory within fixed caps: each file at most 128 MiB, each sound at most 10 minutes of frames at the device rate, and the whole bank at most 512 MiB of decoded i16. The caps are checked against the file size and the frame count the file declares before decoding, and against a running count while decoding. A chart that exceeds them is refused with the reason. Playing long sounds while reading them ahead on a separate thread is built when a chart the player wants to play is refused.
- Each decode worker reads one file, hashes it, drops it at once if the same contents were already taken, and otherwise decodes it and drops the raw bytes before taking the next. A sound being decoded is charged to the bank cap for the PCM it has allocated, before each allocation: its source buffer (allocated exactly when the file declares its length), both buffers while one grows, and the source and converted buffers together while converting. The charge becomes the stored allocation on success, and it and the sound's declared share are returned on failure. So the caps bound the raw file bytes in flight (workers times the file cap) and the decoded PCM (the bank cap). The decoder's own working memory is not bounded (the "Threat Model" section). Sample rates outside 8–384 kHz are not decoded (the sound stays silent with a warning), which bounds the resampler's buffers.
- The real-time audio processing (callback) does neither loading nor decoding. BGA decoding and read-ahead of long sounds run as bounded work on separate threads.
- Polyphony has an upper limit, and the rule for which sound to stop when it is exceeded is decided in advance.
- For sounds that play as BGM, the playback position is computed from chart time and scheduled on the mixer with sample precision.
- BGA (video) is processed on a decode thread, and its display is aligned to the judgment timeline.

### Path from Press to Sound

Key sound latency is the sum of the following segments. Each segment is made measurable, and what can be expected depends on the output path. The device period granted by the audio device is recorded with every measurement, and the game measures and displays the actual latency of the path in use.

- **Standard path (WASAPI shared mode)**: The latency follows from the shared-mode device period P. From judgment to the scheduled mix sample, it is about 2P for the two device periods queued ahead of playback (with one period queued, audio dropped out repeatedly), plus P/2 on average for the wait until the next period event. The mixer resamples nothing during play (the "Aligning Audio to the Judgment Timeline" subsection), so no filter delay is added; a drift-correcting resampler, once built, adds its filter delay (about 1 ms). The OS audio engine and the device's output latency come after this, and the device and input-receipt segments come before judgment. Stage 0 measured a device whose shared-mode period is fixed at 10 ms (`IAudioClient3` offers no shorter period on it), with such a resampler in the path: judgment to the scheduled mix sample was about 26 ms at p50 and 32 ms at p99, which the parts above and that filter delay account for, and no further delay was found. This is how shared mode works on such a device, not a defect.
- **Low-latency path (later stage)**: The 10 ms or less from press to sound applies to a low-latency path built in a later stage: exclusive mode, ASIO, or a device that grants a shorter shared-mode period. Exclusive mode and ASIO stay as described in "Audio" and "Settings Profiles" and are not built before then.

| Segment | What it is | How it is kept down |
| --- | --- | --- |
| Device | Controller scanning and the USB report interval | Depends on the device. Recorded as a device setting |
| Input receipt | Until the OS delivers the input | Receive Raw Input on a dedicated thread |
| Evaluation | Logic thread wake-up and judgment | Wake it immediately on input publication. Judgment processing time is measured, targeting the µs scale |
| Handoff to audio | Waiting until the next buffer write | Lock-free queue. Shorten the buffer period |
| Mixing | Summing, sample rate conversion, limiter | Count the conversion and limiter look-ahead latency in the press-to-sound figure |
| Output | From the written buffer, through the OS and driver, until it sounds from the DAC | Short period in shared mode (standard), exclusive mode (optional). Either way, measure and display the actual latency |

If a "sound to play" event arrives too late for the current buffer write, the sound starts from the first sample that can be written next. The head of the sound is never cut off to make up for the delay. Separately from measuring internal wait times, the actual time from press to sound is also verified with a microphone or dedicated measurement.

## Rendering and Skins

- The play screen is drawn by our own render loop with wgpu. Tool-like screens such as settings may overlay egui.
- The presentation mode (vsync on or off, `present_mode`) and the number of frames rendered ahead (`desired_maximum_frame_latency`) are settings. The defaults favor lower latency.
- Note positions are computed from the time the frame is expected to appear on screen, not the time it is rendered. The expected time is predicted from QPC and the display latency offset, and DXGI frame statistics are used as an aid when valid (the "Time Base (QPC)" section).
- The logic thread publishes a typed state snapshot (for example `PlayView`) through a triple buffer. The render thread only reads it and never writes.
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

From stage 1, the frame time distribution (not just the average, but also the slowest frames) and the latency from input to judgment can be measured and displayed.

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

### Loading and Song Selection

- Key sounds are decoded in parallel on multiple threads. Converted PCM is cached on disk, keyed by file hash, conversion settings, and decoder version. Retries reuse the data in memory. First load and retry are measured separately.
- A library of tens of thousands of songs is stored in SQLite, and only files whose modification time or size changed are re-read. Search uses FTS5, and scanning runs in the background.

### Measurement

- In game, the frame time distribution (p99 and maximum), the lost refresh opportunities (N/A where that count does not apply), GPU time, the latency from input to judgment, and the number of audio dropouts can be displayed at any time.
- A benchmark scene plays back a fixed input log with the worst-case chart and skin, and it is measured regularly on the development PC.
- CPU-side time (evaluator, skin evaluation, loading) is measured as benchmarks (criterion) in CI to detect large regressions. Measurements on shared CI fluctuate, so absolute values are tracked on the development PC.
- For detailed investigation, per-span times are inspected with Tracy (`tracy-client`).

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

## Settings Scopes

| Scope | Owner | Examples |
| --- | --- | --- |
| Device | This PC and this device | The three offsets, debounce, polling, audio device, buffer; with their variations, the OBS connection target (the password is kept in the OS credential store) and whether the local event endpoint is enabled |
| Player | The person playing | Hi-speed, lane cover, the presets and skins used, session policy |
| Preset | The person who authors rules | Judgment windows, gauge, `#RANK` conversion |
| Chart | The chart author | `#RANK`, `#TOTAL`, specification of the LN judgment mode (the preset decides the interpretation and precedence) |

Device and player settings go in separate files, so that latency settings do not get mixed up when several people share one PC or when a device is replaced. There is no team or organization scope. Anything to be shared is distributed as preset or skin files.

Results and replays display and record where the values used came from ("preset X (hash), offsets from the device settings").

## IR (Rankings)

The custom IR receives input logs (replays), not scores. The server recomputes with the same evaluator and builds the rankings.

```
Client ─send→ { start token, chart hash, chart transformations, device settings, input log, versions }
Server ─store→ store the input log as the source of truth
       ─evaluate→ evaluate() with every registered preset
       ─aggregate→ rankings per (chart, preset, evaluator version, metric)
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
| Chart files | Computation breakdown or DoS from a huge note count, extreme BPM or STOP, deeply nested `#RANDOM` | Cap the note count, nesting depth, and time range. Time computation detects overflow (`i128` and checked arithmetic) and rejects loading when exceeded. The same caps are applied on the server | 1 |
| Audio and video | Decoder vulnerabilities | Use maintained decoders and narrow the supported codecs. Accept the remaining risk and keep dependencies up to date. Decoding in a separate process with restricted privileges is decided after measuring whether it is worth the handoff cost | 1 |
| Archives | zip slip, zip bomb | Do not allow writes outside the extraction directory. Cap the extracted size and file count | 1 |
| Dependencies | Supply chain attacks | Run `cargo-deny` and `cargo-audit` in CI, and pin the lockfile | 1 |
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

Windows-dependent code is confined to `input`, `audio`, `render`, and `player`. Linux CI verifies that every other crate builds and passes tests on platforms other than Windows. Therefore, no dependency (including indirect dependencies) from those other crates on these Windows-only implementations is created either. How to split them (conditional compilation or separate crates) is decided at implementation time.

## Key Libraries and Algorithms

Where there is no reason to build our own, established libraries are used. We build our own only for the parts directly tied to the meaning of judgment (the evaluator, chart loading, and time computation) and the parts central to performance (the mixer). Versions and licenses are checked at adoption and managed with `cargo-deny`. Codec, compression, and measurement libraries are not put in `eval`.

### Libraries

| Purpose | Candidate | Decision |
| --- | --- | --- |
| Windows API | `windows` | Use Raw Input, WASAPI, MMCSS, and waitable timers directly. The official bindings |
| Audio output | WASAPI directly via `windows` | Used directly because low-latency shared mode (`IAudioClient3`), exclusive mode, event-driven operation, and fine-grained buffer control are needed. ASIO is an optional feature, supported once the choice of ASIO SDK license (two options: commercial or GPLv3) is settled |
| Audio (alternative) | BASS, BASSmix, BASSWASAPI (BASS\_FX and BASSASIO if needed) | The alternative if our own stack (WASAPI, `symphonia`, `rubato`, our own mixer) hits a dead end in terms of effort or feasibility. It has a track record in rhythm games and provides low-latency playback, scheduling at specified positions on the mixer, and pitch-preserving speed change together. It is a library whose source is not public, and its license terms differ per DLL (BASS is free for non-commercial use with no revenue; BASSASIO has a separate license), so if it is adopted, each DLL's terms and their compatibility with the project's license are checked individually. As conditions for adoption, verify by measurement that the playback position can be mapped to QPC and that clock drift can be corrected. Its latency and dropouts are measured and recorded with the same indicators, compared with our own stack where both run. The commitment not to allocate memory applies only to app-side code; inside the DLLs, it is verified by native heap measurement if needed. DLL versions, where they were obtained, and licenses are managed as dependencies outside Cargo. Audio is enclosed within the boundary of the `audio` crate, so swapping it is done within that crate |
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
| Rational numbers | `num-rational` (`i128`) | Used for time computation. Operations always use the checked versions to detect overflow |
| Library DB | `rusqlite` (bundled SQLite, FTS5) | Song information, scores, search index |
| Archives | `zip`, `sevenz-rust2` | The extraction target and the size caps are always checked. RAR support is postponed because the usable decoders have license constraints |
| Settings and presets | `serde`, `toml` | Files written by people are TOML |
| Replays | Custom binary format, `zstd` | The version number goes first, time deltas are laid out as variable-length integers, and the result is compressed with zstd |
| Hashing and signing | `sha2`, `md-5`, `minisign-verify` | Identification and verification use SHA-256, and MD5 is used only for compatibility with existing difficulty tables. Signatures are unified on the minisign format (distribution registry, auto-update). No cryptography is implemented ourselves |
| Lua (for compatibility) | `mlua` (Lua 5.4) | Used only to convert beatoraja Lua skins at load time. `mlua`'s `sandbox()` is Luau-only, so for Lua 5.4 the allowlist-style environment is built ourselves |
| WASM | `wasm-bindgen` | Calls the evaluator from Workers |
| Measurement and testing | `tracy-client`, `criterion`, `proptest` | Span measurement, benchmarks, property tests of the evaluator |

Code from GPL projects (beatoraja, bmz-player, etc.) and tables ported from them are not used. Behavior is determined from public information and hand-written tests, and the sources of the information behind tables and tests are recorded.

### Scope of No Allocation

The input thread, the logic thread, the audio callback, and the per-frame work of the render thread (app-side code) do not allocate memory during play. The regions for evaluator output, the input log, and effect events are allocated in advance, and what happens on reaching the limit is decided (for the input log, it is recorded as "incomplete"). Background work such as decoding and read-ahead, and allocations inside the GPU driver, are out of scope. The check is done with a test global allocator, which can also be enabled in release builds.

### Algorithms

| Process | Method |
| --- | --- |
| Chart time | Measure positions (rational numbers) are accumulated into time, segment by segment between BPM changes and STOPs. Values with a BPM of 0 or less, a zero denominator, or too many decimal digits are rejected. The order of a BPM change and a STOP at the same position, and how negative values are rounded, are fixed as rules. Rounding to µs is done only once, at the end |
| Scroll position | From BPM, STOP, and scroll speed changes, a piecewise linear function from time to display position is built at load time. STOP segments have a slope of 0. Reverse scrolling is out of scope; on the premise that the display position never decreases with time, the visible range is found per segment by binary search |
| Hi-speed | Two methods are kept distinct. The base-BPM method matches the green number (the time a note is visible) at the main BPM, but the visible time changes when the BPM changes. The constant-visible-time method keeps the green number regardless of BPM. Lane cover and lift positions are included in the visible range calculation |
| Evaluation event processing | Per-lane note deadlines, deadlines for scratch release, debounce, HCN, and song end, and the input stream are taken out in a fixed total order by time. At equal times, inputs come before deadlines (a preset item); inputs keep the merged stream's order (source index, then the order within the source), and deadlines are ordered by kind, then lane, then sequence number. The per-lane streams are already in time order, so this is lighter than pushing everything into a priority queue |
| Note assignment | Each lane keeps a position pointing at its first unprocessed note, and only notes within the judgment window are candidates. How to choose among candidates is decided by a preset choice |
| Debounce | Handled by a small state machine per button. Changes that are too short are dropped, but so that dropping a release does not leave a key held down, the final state always returns to the actual state. When multiple buttons are assigned to one lane, the lane is pressed if any of them is pressed |
| Analog scratch | Axis values are converted into a continuous position, taking wraparound into account, and changes in position are accumulated (for devices that report deltas, the deltas are used as is and not accumulated twice). Separate thresholds for press and release (hysteresis) are used, and a reversal of the rotation direction is treated as a new press. The release at a fixed time after movement stops is handled as an evaluator deadline |
| Gauge | Computed in fixed point (for example, the remaining amount as an `i64` in units of 1/100000%). Intermediate computation is done in `i128`, and the order (compute the increase or decrease → clamp to the bounds) and the handling of remainders that do not divide evenly (floor) are fixed |
| Audio clock correction | Sound starts are scheduled through the frame-to-QPC mapping re-anchored on every period (the "Aligning Audio to the Judgment Timeline" subsection), so drift shows only inside a long running sound, and the measured drift is recorded. When that drift calls for it, a variable-ratio resampler is added: the difference between the device position and the position derived from QPC is measured at regular intervals, smoothed to remove noise, and the conversion ratio is nudged slightly with proportional-integral control. Integral windup is prevented, the ratio and its rate of change are capped, and the controller is reset on discontinuities such as pause and resume. Conversion and limiter latency are counted in the press-to-sound figure |
| Mixing | Preallocated voice slots are reused. Sounds held as i16 (to save memory) are summed in f32, passed through a limiter (the ceiling, attack and release speeds, stereo linking, and amount of look-ahead are decided at implementation time), and converted to the output format |
| Per-chart volume normalization | The sound of playing all key sounds and the BGM with autoplay is analyzed in the background with EBU R128 (gated loudness and true peak) and cached, and from the next play it is applied as a fixed correction. The first load does not wait for it |
| Recommended offset | Only when enough deviations have been collected, the median and the spread are output. This value includes not only device latency but also the player's habits, so it is presented as "a recommendation based on timing bias" |
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
- Given the same section, the same input conversion state at the start of the lead-in, the same lead-in input, the same time mapping, and the same offset changes, the evaluation of a section excerpted from a full input log matches the evaluation of a mid-chart start

Whole-chart evaluation focuses on the fragile spots: inputs exactly on a judgment window boundary, the processing order of events at the same time, mashing, LN release and re-press, and inputs that arrive exactly at the watermark boundary.

From the IR stage on, agreement between the native and WASM builds is verified end to end, not just for the evaluator. From the chart file's bytes and the input log, the resolved chart hash and the judgment event stream are computed on both the native build and the actually deployed Workers, and compared. The loader, evaluator, and preset versions are fixed, and overflow handling and the order of events at the same time follow explicit rules from stage 1. Agreement between incremental and batch evaluation is also verified with input logs that include offset changes and events at the same time.

In addition, each preset has a few dozen small hand-written examples of "this input gives this judgment", to detect unintended changes when presets are tuned. The evaluator is also checked with property tests (generating random input logs).

### Verifying Without Launching the UI

So that each check does not require launching the app and operating it by hand, the whole game is built to run without a screen (the testing mechanisms in the "Overall Architecture" section).

| Target | Method | What it verifies |
| --- | --- | --- |
| Evaluator, chart loading, presets | Unit tests, property tests, hand-written examples, `eval-cli` | Judgment, gauge, time computation |
| Chart loading | Fuzzing with `cargo-fuzz` | Safety against broken files |
| Whole game (headless) | Run one song through at maximum speed, with a virtual clock and a fed input log | Screen transitions, judgment events, sound scheduling positions, retry, practice sections |
| Audio | Generate waveforms offscreen with the same processing as production, with the input PCM, sample rate, initial state, and block length fixed. The sample positions where sounds were scheduled are compared exactly, and waveform values within a tolerance, against the expected output. Including the limiter's look-ahead delay and flushing the tail, also verify that changing how the blocks are divided does not change the result | Mixing, limiter, volume normalization |
| Rendering | Render the screen at a given time offscreen and compare it with the expected image within a tolerance. On Windows CI, the DX12 adapter that renders in software (WARP) is explicitly selected, and the adapter used is recorded. If it cannot be obtained, the test fails rather than being silently skipped. WARP results can also vary by version, so the Windows image, wgpu and the shader translator, fonts, and rendering settings are pinned, and the diff images are reviewed on updates. Presentation mode, VRR, and display latency cannot be verified this way | Skins, note positions, effects |
| Performance | Benchmarks of fixed scenes. CPU side on CI, measurements including the GPU on the development PC | Regressions in the indicators |

Test charts are made with a small mechanism that builds charts in test code and with small self-made BMS text files (to exercise the loading path), and copyrighted charts are not put in the repository. Checks with real charts are done in local environments.

Tests without threads do not find bugs in notifications or queues. So a small number of tests also run on real threads, to verify input order, the watermark, and stop and resume.

### Shortening Launch-Based Checks (Development Builds Only)

- **Scenario launch**: The app can be launched with a scenario (`--scenario scenario.toml`) that describes "load this chart, feed this input log, save the screen at this time, exit". The same scenario is used whether a human or an AI does the checking.
- **Specifying the start position**: From the command line or a scenario, the app can be launched with a specified start position for the practice "mid-chart start" (the "Input Logs and Replays" section). It uses the same mechanism as the product's practice feature, and the development build adds only the way to specify it. It is used in combination with preset and skin reloading.
- **External control** (once the local event endpoint exists): Load, start, and screen-save operations are added to the local event endpoint. The event endpoint is by nature receive-only, so this is an exception for development builds only, and the connection token and loopback restrictions still apply.
- **Measurement output**: On each judgment, the app can flash a square at a fixed position on the screen and emit a test sound (used by the test device below).

### Separating Development and Production Builds

- All of the features above go only into builds with a Cargo feature (for example `dev-tools`) enabled. Rather than hiding them behind a runtime flag, the production build does not contain the code itself.
- The production build is made with `dev-tools` disabled. Features can be enabled even with the `release` profile, so the distribution CI pins the feature configuration. Development command-line arguments (`--scenario`, etc.) do not exist in the production build, and are treated as unknown arguments if passed.
- CI builds the production build and verifies that it does not accept development arguments and that the event endpoint has no development operations.
- Replays and scores made with a development build record that they come from a development build, and cannot be sent to IR.

### Test Input Device

In addition to checks with local controllers, a test input device that an AI can operate is built. The device's design is described in a separate document; here only its role is defined.

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
| 2 | One full gameplay loop: song selection, results, retry, practice (section repeat and speed change), beatoraja-style and LR2-style presets with switching on the result screen, `eval-cli`, and development features such as scenario launch. The test input device is built in this stage or the next | Song selection through play, results, and retry can be played repeatedly. The property tests pass |
| 3 | Look and effects: the declarative custom skin format and polished effects. The LR2 and beatoraja skin adapters come after this, as variations ("Extension Boundaries") | Playing with the standard skin is fun |
| 4 | Custom IR (server-side computation, start registration, impossible-input detection, allowlist) | Input logs can be sent, and rankings for multiple presets can be viewed |
| 5 | Chart registry (catalog only) and installer | Difficulty tables can be installed in one go, and derivative chart dependencies can be resolved |
| 6 | Connecting to existing IRs | Scores computed with the specified preset can be sent in the existing format |

> **Drawing the line on scope**
>
> Until stage 3 is finished, IR, the distribution registry, anti-cheat, and servers are not touched. The only things decided before then in preparation for a future IR and the like are the input log format and the evaluator boundary, so that these can be added later. The later sections of this design document are designs for building at that point, not reasons to build earlier.

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
