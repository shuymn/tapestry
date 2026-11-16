# Test Input Device

A Raspberry Pi Pico 2 that the agent drives as a real USB HID device, for the checks injected input cannot reach. Its role is in [design.md](design.md) "Test Input Device"; this document is its design. The firmware is `test-device/firmware` (outside the workspace); what the firmware and the host share (the protocol, the report layouts, the trace, the pattern format, the UF2 writer, the clock bounds and the matching behind the timing checks) is the `test-device` crate. Both are `MIT OR Apache-2.0` (design.md "Licenses").

## Hardware

The board alone, connected by one USB cable with a micro-USB (Micro-B) plug on the board's side that carries data (many micro-USB cables only charge), to a port directly on the PC. Nothing is wired to the board: the firmware synthesizes every input. The first flash holds the board's BOOTSEL button while the cable goes in, which mounts the boot ROM's drive (`RP2350`); after that the agent reloads the firmware over the cable (Recovery). Parts beyond the board are needed only for end-to-end latency ("Room for Later Uses").

## USB Device

One composite full-speed device, VID:PID `1209:0001`: pid.codes' test product id, for a device that is never distributed. `bcdDevice` is `0x0100 | mode byte`, revision information only: Windows keys interface instances on VID, PID and MI, so a mode change creates no new device instance.

| Interface | Function | Sent |
|---|---|---|
| 0 | HID keyboard, 17-byte report: 8 modifiers, then a bitmap of usages 0-127 | On change |
| 1 | HID game pad (Generic Desktop 0x05), 18-byte report (Reports) | On change; at every poll in continuous reporting, inside a lease only |
| 2-3 | CDC ACM, the control link (Windows' built-in `usbser`, a COM port) | Commands and answers |

Outside a lease the device sends nothing while idle: a controller reporting at every poll would wake a player's input thread at that rate even while idle (design.md "When the input thread wakes"). At configuration it sends one neutral report on each HID endpoint, which identifies the endpoint (Observations).

## Reports

The pad report, little-endian, no report id:

| Byte | Field | Usage |
|---|---|---|
| 0-1 | Buttons 1-16, button 1 in bit 0 | Button page |
| 2 | X, 0-255: the turntable | Generic Desktop X |
| 3 | Mode byte; bit 7 set when bytes 10-17 are valid | Vendor 0xFF00, 0x04 |
| 4-5 | k, the report's sequence number (low 16 bits of the trace's) | Vendor 0xFF00, 0x01 |
| 6-9 | R(k): when report k's content was fixed | Vendor 0xFF00, 0x02 |
| 10-13 | H(k−1): when report k−1 was handed to the controller | Vendor 0xFF00, 0x03 |
| 14-17 | C(k−1): when the host took report k−1 | Vendor 0xFF00, 0x05 |

Times are the low 32 bits of the device's µs timer. The player never sees the vendor fields: it reads controller collections of Generic Desktop usages 0x04 and 0x05 and skips value caps off the Desktop page. The crate checks each layout against its own descriptor by parsing it (F0).

**Report rate.** The interrupt endpoints' `bInterval`: 1 for 1000 Hz, 8 for 125 Hz. The mode byte holds the interval in ms in its low nibble and continuous reporting in bit 4. `mode` stores the mode in a watchdog scratch register and reboots; the host then rediscovers the device.

**Wraps.** The 16-bit sequence number wraps every 65 536 reports (65.5 s at 1000 Hz, 8.7 min at 125 Hz); the host unwraps it monotonically, and a jump of 32 768 or more is a failure, not a wrap. The 32-bit times wrap every 2³² µs (about 71.6 min); the host places each in the 2³² window nearest the clock mapping's estimate. Pongs and the trace carry 64-bit µs.

## Observations

embassy-rp's `EndpointIn::write` waits until the previous buffer's AVAILABLE bit clears, copies the report, sets AVAILABLE and returns: its return is a handoff, not a completion. So each report has three observations on the device's µs timer:

| Observation | When |
|---|---|
| R(k), ready | Report k's content is fixed, after the previous report completed |
| H(k), handoff | `write` returns: AVAILABLE is set and the host's next IN token can take the report. design.md's "press time" |
| C(k), completion | The firmware sees the controller clear AVAILABLE, polling the buffer control in DPRAM and yielding between polls |

- **Endpoint identity.** The firmware reads the endpoints' buffers by index (keyboard 1, pad 2). After each endpoint's first write, its buffer control's LENGTH must equal its report length (17 or 18, unique among the endpoints). Until both hold, `arm` and `trace on` are refused.
- **Association.** Each endpoint keeps whether a report is pending as its own state. "A previous report exists" is never read off a nonzero sequence number, which would skip the completion at every wrap.
- **What a claim may use.** A timing claim uses H or C, never R; and only after F-S has tested these semantics on hardware.

## Timing Trace

`trace on` streams numbered records over the link, interleaved with answers (records start with an uppercase letter, answers with a lowercase word). The trace is authoritative for both endpoints: in on-change reporting a last report has no next report to carry its H and C, and the keyboard carries none.

| Record | Meaning |
|---|---|
| `T <n> <ep> <seq> R <µs> H <µs>` | Report `seq` on endpoint `k` or `p`, fixed at R, handed at H |
| `C <n> <ep> <seq> <µs>` | Its completion |
| `X <n> <ep> <seq> <available> <length> <µs>` | A revocation, with the buffer control read at `EP_ABORT`; `available 0` means the host took the report first |
| `L <n> lost <count>` | Records dropped because the ring was full |
| `P <n> <ep> <seq>` | Still pending when `flush` gave up |
| `F <n>` | The end of a `flush` |

Sequence numbers are 32-bit per endpoint; times are 64-bit µs. Records go through a 1024-record ring, and the numbers n are consecutive, so a gap is a loss; any loss fails the row. `flush` waits until every endpoint's pending report completes, bounded by two intervals plus 10 ms, then emits `P` for each still pending, the buffered records and `F`. A row needs `F` and no `P`. The pad's in-report fields must agree with the trace.

**Keyboard edges.** Each keyboard record is one pattern item, a known set of usages going down or up. The listener's edges from the device's handle are matched to records in order across records and as a set within one (the order Windows emits them is unspecified). A make of a key already down is a typematic repeat: it has no record and is classed apart. Missing or extra edges fail. Pad timing never stands in for keyboard timing.

## Commands

One command per line; one answer per line (`ok`, `err <reason>`, or the answers below).

| Command | Effect |
|---|---|
| `hello` | `hello <version> build <id> mode <byte> lease <id\|-> armed <0\|1> reason <reason> boot <cause> count <n> identified <0\|1>` |
| `mode <1000\|125> <change\|continuous>` | Stores the mode and reboots |
| `lease <ms>` | A lease of at most 30 s: `lease <id> <deadline µs>`; refused while one is held |
| `renew <id>` | Extends the lease by its own length |
| `key <usage> <0\|1>`, `button <1-16> <0\|1>`, `axis <0-255>` | One held change under the lease, sent as its own report |
| `load`, then `item <µs> <change>...` | A pattern: items of up to 8 changes on one endpoint (`k<usage>+`, `k<usage>-`, `b<n>+`, `b<n>-`, `x<value>`), at µs after the start, in order per endpoint |
| `arm <lease id> <device µs>` | Plays the loaded pattern from that time |
| `ping <n>` | `pong <n> <µs>`: the device's 64-bit µs, sampled after the ping is read and before the pong is queued |
| `trace on\|off`, `flush` | The timing trace |
| `release` | Ends the lease, releasing everything |
| `bootsel` | Reboots into the boot ROM's drive |
| `detach <ms>` | Takes the device off the bus (the pull-up off) for up to 10 s |
| `fault <name> [arg]` | A debug fault; `err no-faults` in a normal build |

**Patterns.** Each item is sent as one report, and the writer waits for the previous report's completion before fixing the next, so a change is never overwritten before its report is sent (design.md "Test Input Device"). An item of several changes makes one report with all of them.

## Lease and Disarm

Every held input and every pattern belongs to a lease. A timer of its own checks the deadline every 0.5 ms; it does not depend on the link's reads or on the HID writers.

- **Disarm triggers:** expiry, DTR dropped or the port closed, a bus suspend, a bus reset, `detach`, `release`.
- **On disarm, in this order:**
  1. Drop every queued pattern item and held change, and the held state.
  2. Revoke a report handed but not yet taken on each HID endpoint: set `EP_ABORT`, wait for `EP_ABORT_DONE` (bounded), clear AVAILABLE and set the buffer's DATA PID back to the last delivered report's, then clear `EP_ABORT`. The next write sends the opposite PID of what the register holds, which is then the toggle the host expects. Each revocation is traced as `X`.
  3. Hand a neutral report (everything released, the axis unchanged) on each endpoint whose last delivered report was not neutral, whose taken report was not neutral, or whose report was just revoked. A delivered report after a revocation is what shows the toggle was restored.
  4. Stay disarmed: nothing plays until a new `lease` and `arm`. A pattern never resumes after expiry, suspend or reset; an ended lease's id arms nothing.
- **On resume** the first report the host takes is the neutral one.
- The host keeps DTR asserted while it holds the port.

## Recovery

Each path works without the button, and each is tested on its own (F5):

- `bootsel` calls the boot ROM's `reboot` into BOOTSEL.
- A line coding of 1200 baud does the same, handled where the link's control lines are watched, independent of the command parser.
- A watchdog of 2 s, fed by a future of its own in the one executor task, resets a firmware whose futures stop yielding (or that panicked).
- A scratch register counts consecutive watchdog boots; at 3 the firmware reboots into BOOTSEL. The counter clears at any boot the watchdog timer did not cause (power-on, a forced reboot), and after 10 s of running with a `hello` answered. `hello` reports the boot's cause and count.

Scratch registers 0-2 hold the mode, the counter and the boot-hang fault; every path into BOOTSEL clears the last two. If every path fails on hardware, the owner is asked once to replug the board with BOOTSEL held.

## Debug Faults

The firmware's `debug-faults` feature adds `fault <name> [arg]`, the negative rows' mutations. Each fault clears itself 30 s after it is set, so no negative row can leave the board stuck; `boot-hang` clears after its count of boots, and `clock-offset`, which breaks nothing, lasts until the next reboot. A normal build answers `err no-faults` and sets nothing.

| Fault | Effect | Row |
|---|---|---|
| `c-at-write` | The handoff recorded as the completion | F-S |
| `every-other-poll` | Skips a poll after each report | F2, U5 |
| `mode-byte-only` | Reports the other rate's mode byte, `bInterval` unchanged | F3 |
| `clock-offset` | Reported µs start 10 s before 2³² | F4 |
| `pong-offset <µs>` | Shifts each pong's µs | F4 |
| `parser-off` | Ignores every command | F5a |
| `baud-off` | Ignores the 1200-baud line coding | F5a |
| `hang` | Spins without yielding | F5b |
| `watchdog-8s` | Feeds the watchdog with 8 s | F5b |
| `boot-hang <n>` | Hangs now and at each of the next n boots | F5c |
| `ignore-disarm` | Every disarm does nothing | L1 |
| `suspend` | Runs the suspend disarm | L3 |
| `keep-armed` | Bus disarms (detach, suspend, reset) do nothing | L4 |
| `drop-item <n>` | Never sends pattern item n | U1 |
| `split` | Sends each change of an item as its own report | U2 |

## Clock Bounds

Device and host clocks differ in origin and rate, and round trips cannot fix the one-way delay (design.md "Test Input Device").

- **Model:** host time = a + b·(device time), assuming both clocks keep constant rates over a run (the device's crystal and the host's counter, over minutes); the verdict states the assumption. b is taken within 1000 ppm of 1.
- **Feasible set:** each ping gives s_i ≤ a + b·d_i ≤ r_i (s_i the host's send, d_i the pong's µs, r_i the host's receipt). The (a, b) satisfying all of them form a convex polygon, computed exactly: each vertex is the meeting of two constraint lines, held as i128 fractions. A device time maps to the interval of a + b·t over the vertices, rounded outwards.
- **Inconsistency:** an empty polygon fails the run as inconsistent; no observation is dropped, and the verdict names the pings whose removal alone would restore it.

Fitting b first and then intersecting the offsets that b allows is not used: when the delay asymmetry changes during a run, the fitted rate is biased (36 ppm for 3 ms out and 0.1 ms back reversing halfway through 120 s), and the offsets then exclude the truth. A unit test keeps both facts.

## Verdicts

The `test-device` crate's `matcher` holds the comparisons, each failing with the first thing that breaks its rule:

- **Edges** (U1(a), U4): matching as in "Keyboard edges"; "missing edge k" or "extra edge".
- **Batches** (U2): the edges of one report share one receive time. A necessary condition only.
- **Spacing** (U1(b)): recorded intervals against handoff intervals, within one poll period, 125 µs and the listener's spread.
- **Differential** (U1(c)): d = player stamp − listener stamp for the same report, both on the host's counter; |d| ≤ 1 ms, two-sided, an empirical target recorded with its environment. It is not a latency and bounds no absolute delay.
- **Pairing** (F-S): receipt − map(C(k)) meets [0, L] with L = 4 ms (half the 125 Hz interval), and the mapping is narrower than L, so pairing report k with C(k−1) misses by one interval.
- **Latency** (F4): every receipt − map(C) interval meets [0, 20 ms].

## Host Commands

The host side is `tapestry test-device <command>` in `player`, behind `dev-tools` (Windows-only, GPL):

- `devices`: the composite parent `USB\VID_1209&PID_0001` and its interfaces through the configuration manager, the Raw Input handles by interface path and container id, the COM port of interface 2, and the active configuration descriptor (each endpoint's `bInterval`) from the hub.
- `flash <uf2>`: `bootsel` (or the 1200-baud touch), the `RP2350` drive within 10 s, the copy, then rediscovery until `hello` reports the new build id.
- `listen --out <jsonl>`: a hidden window registered for the pad and keyboards with `RIDEV_INPUTSINK`; per report the host counter, the handle, k, R, H, C and the controls; keyboard edges from the device's handle. Handles are reacquired after every arrival and removal.
- `trace --out <jsonl>`: keeps the link open for a run, records the trace and the pings, and ends with `flush`.
- `run <run.toml>`: drives a run of the release player with a scratch `LOCALAPPDATA`: focus, lease, holds, patterns, pointer drags and clicks, client-area shots, waits on the player's output; rediscovers after any reconnect.
- `compare`: decodes the record and judges with the crate's matching and clock bounds, one JSON verdict per check.

Panel judges that need a skin's geometry or reference captures stay outside the repository.

## Building

```sh
mise exec -- cargo xtask test-device build [--features debug-faults] [--picotool]
```

This builds the firmware with the pinned Rust and the `thumbv8m.main-none-eabihf` target (both from `mise.toml`), converts the ELF's loadable segments into a UF2 for the `rp2350-arm-s` family with the crate's writer, and writes it to `<target dir>/test-device/firmware.uf2` (`firmware-faults.uf2` with the faults). It refuses an image with no image definition in its first 4 KiB, where the boot ROM looks for it. The build id the firmware reports in `hello` is the commit, with `-dirty` for uncommitted changes and `-faults` for a debug build. `--picotool` also runs picotool, from the Nix `test-device` shell (docs/setup.md "Linux-Side Tools (WSL)"): `picotool info -a` must name the RP2350 and an ARM Secure image, and picotool's own conversion of the ELF must be byte-identical. The firmware's format and clippy run from its own folder, whose `.cargo/config.toml` sets the target:

```sh
mise exec -- cargo fmt --manifest-path test-device/firmware/Cargo.toml --check
cd test-device/firmware && mise exec -- cargo clippy --release --locked [--features debug-faults] -- -D warnings
```

CI builds both UF2s on Linux and checks the normal one with picotool in the Nix shell; no CI job needs the device.

**Why embassy.** The firmware uses embassy-rp and embassy-usb: it builds with the pinned toolchain on Windows and shares one tested definition of the protocol and the report layouts with the host. The Pico SDK with TinyUSB has the longer field use on this chip, a reset interface and a completion callback, but needs a second toolchain (unpinned on Windows) and a second implementation of the protocol. It stays the fallback if embassy fails F-S, F1 or F3 on hardware, built in the Nix shell against this document.

## Verification

Every row is judged by a script; an owner's look never passes one. Real-app rows run the release player with a scratch `LOCALAPPDATA` and `device.toml` (the device's controller: buttons 1-7 on lanes 1-7, X on the scratch, buttons 9 and 10 on START and SELECT).

| Row | Checks | Negative row |
|---|---|---|
| F0 (CI) | The image definition in the first 4 KiB; picotool names RP2350 and ARM Secure; the UF2 byte-identical to picotool's; each layout matches its descriptor; round trips; unwraps across both wraps | A build without the image definition is refused; a descriptor field's width changed fails the layout test at that field |
| F-S | 125 Hz, 500 changes: C − H in (0, 8000] µs with the minimum below 500; pairing in [0, L]; the trace agrees with the reports | `c-at-write`; pairing C(k−1) with report k |
| F1 | Enumeration: the parent, the keyboard and pad interfaces, the report length, the COM port answering `hello`; one controller and one keyboard source in a record; each pattern usage's scan code | `bootsel`: the parent and children are missing |
| F2 | 1000 Hz, 120 s continuous: `bInterval` 1, median interval 1000 ± 20 µs, gap-free across the sequence wrap, C − H ≤ 1050 µs | 125 Hz expectations; `every-other-poll` |
| F3 | 1000 → 125 → 1000 three times: everything rediscovered after the last arrival; `bInterval` and medians match the mode byte | Skipped rediscovery; `mode-byte-only` |
| F4 | 120 s of pings with the time-wrap offset: the feasible set is not empty; every receipt − map(C) meets [0, 20 ms] | Unit tests: asymmetric delays and ±200 ppm keep the truth; a pong 5 ms late is named inconsistent; pongs 50 ms early end every interval below 0 |
| F5a-c | 1200 baud; watchdog; three watchdog boots reach the drive, a healthy run between hangs does not | `baud-off`; `watchdog-8s`; a counter never cleared |
| L1-L4 | The host's process killed, a silent port, suspend and resume, a detach mid-pattern: releases within the lease plus one interval, neutral first after resume, nothing until rearmed, `hello`'s reason | `ignore-disarm`; the expiry check moved into the link's reads; no neutral before resume; `keep-armed` |
| R1 | Revocation with its phase controlled: a sentinel held in report k0, report k1 handed 2 ms after C(k0) and disarmed 2 ms later; `X` shows k1 queued; k1 never arrives; the sentinel's break arrives; a probe armed before the disarm never plays before the rearm | No revocation; no toggle restore; no rearm required |
| P1-P6 | The selection panel opened by START and SELECT held: through the controller and the keyboard, against injected input and a revert; the list under the panel stays put | Each judged by its own assertion |
| U1-U5 | Timing on real plays at both rates: every edge once and in order, spacing, the differential, batches, fast repeats, the keyboard on its own trace, recorded intervals | `drop-item`, swapped edges, a shifted player time, a changed time in a batch, deleted and inserted edges, `every-other-poll` |
| U6 | The polling-rate estimate, the low-rate warning and "not estimable" | Waits for the estimator (design.md "What Determines Timing Accuracy") |

## Room for Later Uses

- **The turntable as an encoder** needs no part: mode entries for relative or absolute axes of 8 or 16 bits, pattern items for speed and direction profiles. It does not replace real controllers' quirks (design.md "Test Input Device").
- **End-to-end latency** needs parts to buy: an analog light sensor module (an OPT101-type or TEMT6000 breakout) for the measurement square into the ADC; a 3.5 mm breakout with bias resistors, a coupling capacitor, a series resistor and clamp diodes for the audio output; a breadboard and jumpers; pin headers unless the board has them; and an opaque mount for the sensor. The firmware would sample the ADC on the second core with DMA on the same µs timer and stream detections over the link.
