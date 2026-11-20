# Test Input Device

A Raspberry Pi Pico 2 that the agent drives as a real USB HID device, for the checks injected input cannot reach. Its role is in [design.md](design.md) "Test Input Device"; this document is its design. The firmware is `test-device/firmware` (outside the workspace); what the firmware and the host share (the protocol, the report layouts, the trace, the pattern format, the UF2 writer, the clock bounds and the matching behind the timing checks) is the `test-device` crate, and so is the device's logic (`device`: held input, patterns, the lease and its disarm, revocation, the link's framing, the revocation trial), which the firmware runs on the USB controller and the crate's tests run against a simulated controller and host. Both are `MIT OR Apache-2.0` (design.md "Licenses").

## Threat Model

The harness (the device's firmware, the host commands, the recorder, the judges and the checker) must stay safe and its verdicts correct against:

- **its own code's bugs, crashes and stalls**, on the host and in the firmware;
- **USB and OS events**: a disconnect, a device reboot, an unconfiguration, a suspend, a failed read, a drive letter that changes or passes to another volume;
- **stale or mismatched evidence files**: a folder or file from another run, a run judged against another row's expectation, a capture taken from another set.

Out of scope, recorded as a limit: **deliberate tampering**, such as a build with modified sources that reports a clean build id, or evidence edited by hand. Reviews judge findings against this statement.

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

Bytes 10-17 are valid (bit 7 of byte 3) exactly when the host took report k−1: not in the first report, and not after report k−1 was revoked, which has no completion. Times are the low 32 bits of the device's µs timer. The player never sees the vendor fields: it reads controller collections of Generic Desktop usages 0x04 and 0x05 and skips value caps off the Desktop page. The crate checks each layout against its own descriptor by parsing it (F0).

**Report rate.** The interrupt endpoints' `bInterval`: 1 for 1000 Hz, 8 for 125 Hz. The mode byte holds the interval in ms in its low nibble and continuous reporting in bit 4. `mode` stores the mode in a watchdog scratch register and reboots; the host then rediscovers the device.

**Wraps.** The 16-bit sequence number wraps every 65 536 reports (65.5 s at 1000 Hz, 8.7 min at 125 Hz); the host unwraps it monotonically, and a jump of 32 768 or more is a failure, not a wrap. The 32-bit times wrap every 2³² µs (about 71.6 min); the host places each in the 2³² window nearest the clock mapping's estimate. Pongs and the trace carry 64-bit µs.

## Observations

embassy-rp's `EndpointIn::write` waits until the previous buffer's AVAILABLE bit clears, copies the report, sets AVAILABLE and returns: its return is a handoff, not a completion. So each report has three observations on the device's µs timer:

| Observation | When |
|---|---|
| R(k), ready | Report k's content is fixed, after the previous report completed |
| H(k), handoff | `write` returns: AVAILABLE is set and the host's next IN token can take the report. design.md's "press time" |
| C(k), completion | The firmware sees the controller clear AVAILABLE, polling the buffer control in DPRAM and yielding between polls |

The controller clears AVAILABLE when the host's ACK reaches it, and only then does a cleared bit mean the host took the report. embassy-rp also clears it, with no ACK, whenever it enables or disables an endpoint: `SET_CONFIGURATION` (1 or 0) and `SET_INTERFACE` rewrite the buffer control to AVAILABLE clear and the register's PID DATA1, so the next report goes as DATA0, where the host restarts the endpoint's toggle. embassy-usb tells the USB handler of each in the same synchronous step, before any completion poll can run. A pending report's completion can then no longer be observed: the host may have taken it with an ACK just before the rewrite, or not at all, and the report counts as dropped, never taken, either way (Lease and Disarm). embassy-usb also reports a bus reset's return to alternate setting 0, which touches no endpoint; the logic ignores it until the next configuration. A bus reset only disables the endpoints and leaves their buffers as they are; the disarm it causes revokes the pending report, and the configuration that follows reinitialises the endpoints. Nothing else clears AVAILABLE: an abort leaves it to the firmware.

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
| `A <n> <ep> <seq> <µs>` | The controller did not acknowledge the abort of report `seq`: it was not revoked, and the device reboots (Lease and Disarm) |
| `E <n> <ep> <seq> <µs>` | Report `seq` was pending when the host configured the device or its interface again: the rewrite left its completion unobserved, whether or not an ACK came just before, so it counts as withdrawn, not taken (Observations) |
| `V <n> <ep> <trial> <k0> <k1> <probe µs>` | A revocation trial's reports, at k1's handoff (Revocation Trial) |
| `L <n> lost <count>` | Records dropped because the ring was full |
| `P <n> <ep> <seq>` | Still pending when `flush` gave up |
| `F <n>` | The end of a `flush` |

Sequence numbers are 32-bit per endpoint; times are 64-bit µs. Records go through a 1024-record ring, and the numbers n are consecutive, so a gap is a loss; any loss fails the row. `flush` waits until every endpoint's pending report completes, bounded by two intervals plus 10 ms, then emits `P` for each still pending, the buffered records and `F`. A row needs `F`, no `P` and no `A`. The pad's in-report fields must agree with the trace: R(k) with report k's `T`, and bytes 10-17 with report k−1's `T` and `C` when the trace shows k−1 taken, absent when it shows k−1 revoked or dropped (k−1 of report 0 is the last before the 32-bit wrap; a k−1 handed before the trace began is not checked).

**Keyboard edges.** Each keyboard record is one pattern item, a known set of usages going down or up. The crate's pinned usage-to-scan-code table (`keys`: letters, digits, Enter, Escape, Backspace, Tab, Space, F1-F24, the arrows and six modifiers) maps them to make and break codes; F1 reads each pattern usage alone on hardware once against it. The listener's edges from the device's handle are matched to records in order across records and as a set within one (the order Windows emits them is unspecified). A make of a key already down is a typematic repeat: it has no record and is classed apart. Missing or extra edges fail. Pad timing never stands in for keyboard timing.

## Commands

One command per line; one answer per line (`ok`, `err <reason>`, or the answers below). This is protocol version 2: a `hello` of another version is refused by the host.

**One wire form.** A line ends at a line feed and nowhere else. It is a command only in the form the host writes: the crate's `Command::parse_canonical` accepts a line exactly when the command's own text writes the same bytes back, that is words of printable ASCII separated by single spaces, nothing before the first word or after the last, integers with no leading zeros and no `+`, a `-` only on a signed field (`fault pong-offset -50000`), a token in 24 lowercase hex digits. The host writes every command through the crate's `protocol::encode`, which checks the 160-byte limit. Any other line, a carriage return anywhere and an empty line included, is answered `err syntax`, as is one that is not UTF-8, and one longer than 160 bytes `err overlong` (once, at its first byte past the limit; the rest of the line is dropped): each is answered once and ends the lease (Lease and Disarm). The host reads answers and trace records the same way: a line its type would not write back unchanged is malformed. Answers and records may be longer than commands (`hello` above all): they are at most 255 bytes with their line feed (`protocol::MAX_ANSWER`).

**Connections.** A command acts only within the link connection that sent it. The connection ends with DTR dropped (the port closed), a failed read, a bus reset, an unconfiguration or `detach`; the device takes DTR and the line coding from the control requests as they arrive, whatever the link's reader is waiting on. At the end the partial line is dropped, and so is the rest of a packet whose earlier command the device was still running (a `flush` waiting, say), so no command completes in the next connection. Output of the ended connection that the link's IN endpoint still holds is withdrawn at the end itself, before anything else the device does (an abort of its buffer, as a disarm revokes a report), at every end. A grant past the connection's last number (2³² grants in one connection) does not end the connection in place: the device reboots through the watchdog, answering nothing, so it leaves the bus and no reply of that connection can reach a later read on the same port (Recovery). The next connection's input is acted on only after the withdrawal; when the controller does not acknowledge the abort, it waits, neither acted on nor answered, until the device reboots (Recovery). A packet counts as the connection in which the device reads it. Each connection has a session token: 64 bits of the boot ROM's per-boot random number, then the connection's 32-bit count in that boot, written as 24 lowercase hex digits. The boot ROM makes that number before the firmware runs, by hashing raw samples of the TRNG's ring oscillator with SHA-256 (RP2350 datasheet 12.12.4.1), and `get_sys_info` returns it as `BOOT_RANDOM`, "a 128-bit random number generated on each boot" (5.4.8.17). It "will change on every core 0 reset (except debugger skipping ROM)" (12.12.4.1), and every reboot of the device (the watchdog's, a forced one, the drive's flash update) runs the boot ROM; the firmware does not drive the TRNG itself. The count never wraps. A boot whose read fails (`get_sys_info` returns an error or does not report `BOOT_RANDOM`, or the 64 bits are all zeros or all ones) has no token, nor has one that has spent the count (2³² connections), until a reboot reads again: its `hello` reads `session -` and it refuses every grant `err session`.

| Command | Effect |
|---|---|
| `hello` | `hello <version> build <id> mode <byte> lease <n\|-> armed <0\|1> reason <reason> boot <cause> count <n> identified <0\|1> faults <0\|1> quiet <0\|1> session <token\|->`. `lease` is the held lease's number within the connection. `faults` is 1 exactly in a `debug-faults` build. `quiet` is 1 exactly when no lease is held, nothing is armed or playing, and on each endpoint nothing is held, queued, pending or owed and the last report the host took was neutral (the axis keeps its position and does not count). `session` is the connection's token |
| `mode <1000\|125> <change\|continuous>` | Stores the mode and reboots |
| `lease <token> <ms> <deadline µs>` | A lease of at most 30 s for the connection `token` names, its first deadline at that device time (as `ping` reports it): `lease <id> <deadline µs>`, the id written `<token>:<n>` with n counting the connection's grants; n never wraps: a grant past 2³²−1 reboots the device, unanswered (Connections, above). `err session` for another token, `err stale` when the deadline is not after the device's time as it reads the command, `err bad-length` when it lies further than `ms` from then; `err held` while one is held |
| `renew <id> <deadline µs>` | Moves the lease's deadline: `lease <id> <deadline µs>`; `err session`, `err stale` and `err bad-length` as for `lease` |
| `key <id> <usage> <0\|1>`, `button <id> <1-16> <0\|1>`, `axis <id> <0-255>` | One held change under lease `id`, sent as its own report; `err no-lease` with none held, `err wrong-id` when `id` is not the held lease's |
| `load`, then `item <µs> <change>...` | A pattern: items of up to 8 changes on one endpoint (`k<usage>+`, `k<usage>-`, `b<n>+`, `b<n>-`, `x<value>`), at µs after the start, in order per endpoint |
| `arm <id> <device µs>` | Plays the loaded pattern from that time under lease `id` |
| `ping <n>` | `pong <n> <µs>`: the device's 64-bit µs, sampled after the ping is read and before the pong is queued |
| `trace on\|off`, `flush` | The timing trace |
| `release` | Ends the lease, releasing everything |
| `bootsel` | Reboots into the boot ROM's drive |
| `detach <ms>` | Takes the device off the bus (the pull-up off) for up to 10 s |
| `revoke-test <id> <k\|p> <trial>` | One revocation trial on the endpoint under lease `id`, which it ends (Revocation Trial); `err mode` unless 125 Hz on change, `err playing` while anything is armed, held or owed |
| `fault <name> [arg]` | A debug fault; `err no-faults` in a normal build |

**Patterns.** Each item is sent as one report, and the writer waits for the previous report's completion before fixing the next, so a change is never overwritten before its report is sent (design.md "Test Input Device"). An item of several changes makes one report with all of them.

## Lease and Disarm

Every held input and every pattern belongs to a lease. Device input's safety rests on one rule the device enforces:

1. **A connection's end ends its lease and its output.** Every path that ends a connection goes through one function of the crate, which ends the lease, turns the trace off and drops its buffered records: DTR dropped or the port closed, and a failed read (`closed`), a bus reset (`reset`), an unconfiguration (`unconfigured`), `detach` (`detached`). A grant past the connection's last number ends it by a reboot instead (Commands), which ends everything the host holds from the device with it. A suspend does not end the connection; it disarms (`suspended`). Every answer, trace record and `flush` carries its connection to the USB handoff. In each poll of a packet's write the firmware's sender reads the IN buffer's AVAILABLE, then the crate decides (`State::link_write`), the connection first: a packet of a connection that is no longer the current one is abandoned whatever the buffer read, and a current connection's packet is copied to the buffer and AVAILABLE set in that same poll when the buffer was free. So a line or a `flush` of an ended connection is abandoned there, a packet still waiting for the IN buffer included, even when the buffer came free in the same instant. A packet it already handed the IN buffer is withdrawn at the end itself (Commands), so no byte of one connection reaches another.
2. **A grant names the connection and a deadline.** `lease` acts only for the current connection's token and sets an absolute deadline on the device's clock, which the host derives from a check it made; `renew` moves it. Neither sets a deadline that has passed or lies further than the lease's length from the device's time as it reads the command, so a grant or renewal delayed on its way can only end the lease sooner.
3. **Every held change names its lease.** `key`, `button`, `axis`, `arm` and `revoke-test` act only under the lease their id names. The host can name only a lease its own connection's answer gave it, so nothing from an ended connection or an earlier boot acts.
4. **Authority ends at the deadline, checked where it is used.** Before the device accepts a renewal or a held change, as a writer asks for its next report, and as it commits a report, it ends a lease whose deadline has passed (`expired`). Each decision reads the clock once: a renewal's expiry check and its new deadline's bounds use the same sample. A report is staged when its writer decides it and committed once the endpoint's buffer is free, in one critical section that samples the clock, checks the deadline against that sample with a guard band, builds the report stamped with it, copies the report to the endpoint's buffer and sets AVAILABLE (`State::commit`, the firmware's `device::commit`), so nothing runs between the check and the handoff. A pressed report commits only while the sample plus the margin (`device::COMMIT_MARGIN_US`, 100 µs) is before the deadline; otherwise the lease ends there (`expired`) and the owed neutral report commits instead, as does any report committed while a release is owed. The margin covers what follows the sample inside the critical section: building the report, the copy and the AVAILABLE write. Its basis is a measurement on one board at the 150 MHz system clock, interrupts off, from the clock sample to the AVAILABLE store's completion: at most 31.8 µs on the first of 10 000 runs at boot, whose instructions came from flash through a cold cache (the others down to 2.0 µs), and at most 21.7 µs in the HID writers' commits. A sample lags the true time by up to 1 µs, as the µs timer truncates; the margin is about three times the sum. Its cost: a press in a lease's last 100 µs reports neutral, and renewals come long before. A timer of its own also checks the deadline every 0.5 ms, ending a lease that nothing else touches; it does not depend on the link's reads or on the HID writers, and the watchdog is fed only while it runs (Recovery).

**The release property**, in two parts. (a) *Authority cutoff:* no held change acts, no renewal extends and no pressed report is handed after the last deadline the device accepted, nor after the connection that set it ends. (b) *Host-visible release:* after the cutoff, while the endpoint is polled and the firmware makes progress, the host has the release within two successful polls of that endpoint: the first owed neutral report, or the second when the host dropped the first as a repeat after a pressed report's lost ACK. While the endpoint is not polled (a suspend, an unconfiguration) or the firmware does not progress (a stalled timer, until the watchdog resets the device), the release comes at resume, at the next configuration, or with the device's removal (a reboot, the unacknowledged-abort recovery, the watchdog). No time is claimed for (b): the crate's tests show both parts on the simulated clock and USB (a lost ACK, a suspend, an unconfiguration, a stalled timer).

- **Disarm triggers:** expiry, the end of the connection (above), a line that is not a command (malformed, not UTF-8 or too long), a bus suspend, `release`.
- **On disarm, in this order:**
  1. Drop every queued pattern item and held change, and the held state.
  2. Revoke a report the controller still holds on each HID endpoint: set `EP_ABORT`, wait for `EP_ABORT_DONE` (at most 100 000 reads), clear AVAILABLE and set the buffer's DATA PID back to the last delivered report's, then clear `EP_ABORT`. The next write sends the opposite PID of what the register holds, which is the toggle the host expects if it never took the withdrawn report. The controller clears AVAILABLE only when the host's ACK reaches it, so the host may also have taken the report and the ACK been lost (USB 2.0 §8.6.4); the host then expects the other PID and would discard the next report as a duplicate. Each revocation is traced as `X`. Without the acknowledgement nothing is touched or claimed: the buffer and `EP_ABORT` stay as they are, the report stays pending, the trace records `A`, the device sends nothing more and reboots 20 ms later, which takes it off the bus and ends every input the host holds from it.
  3. Owe a neutral report (everything released, the axis unchanged) on each endpoint whose last delivered report was not neutral or whose taken report was not neutral, and two on an endpoint whose report was just revoked: one of two reports in a row carries the PID the host expects, whichever it expects, and after both the toggles agree again. The owed reports go before anything else on their endpoint, and no held change, pattern item or trial report goes until the host has taken them; one revoked in its turn by another disarm makes two owed again. The release arriving with the first of the two, when the host never took the withdrawn report, is what shows the toggle was restored.
  4. Stay disarmed: nothing plays until a new `lease` and `arm`. A pattern never resumes after expiry, suspend or reset; an ended lease's id arms nothing.
- **On resume** the first report the host takes is the neutral one.
- **When the host configures the device or an interface again** (Observations), each endpoint it reinitialises drops its pending report (`E`); a reconfiguration alone leaves the lease as it is. A neutral report is owed on it, as in step 3, when the host may still hold something: the last delivered report or the dropped one was not neutral. Reports already owed stay owed, a dropped one included. They go ahead of any queued input, and the current state follows them.
- The host keeps DTR asserted while it holds the port.

## Recovery

Each path works without the button, and each is tested on its own (F5):

- `bootsel` calls the boot ROM's `reboot` into BOOTSEL.
- A line coding of 1200 baud does the same: the USB handler takes it as the request arrives, independent of the command parser and of what the link's reader waits on, and the lease's timer starts the reboot at its next tick.
- A watchdog of 2 s resets a firmware whose futures stop yielding (or that panicked). A future of its own in the one executor task feeds it every 250 ms, and only when the lease's timer has run since the last feed: a stalled timer, which may leave a lease past its deadline to nothing else while the writers are idle, ends in a reset at most 2.25 s after the stall. The crate holds these times and the feeding rule (`device::WATCHDOG_US`, `State::feed_due`).
- A scratch register counts consecutive watchdog boots; at 3 the firmware reboots into BOOTSEL. The counter clears at any boot the watchdog timer did not cause (power-on, a forced reboot), and after 10 s of running with a `hello` answered. `hello` reports the boot's cause and count.

Scratch registers 0-2 hold the mode, the counter and the boot-hang fault; every path into BOOTSEL clears the last two. An abort the controller does not acknowledge, of a report or of an ended connection's output, reboots the device through the watchdog (a forced reset, which clears the counter; Lease and Disarm), and so does a grant past a connection's last number, at once (Commands). If every path fails on hardware, the owner is asked once to replug the board with BOOTSEL held.

## Debug Faults

The firmware's `debug-faults` feature adds `fault <name> [arg]`, the negative rows' mutations. Each fault clears itself 30 s after it is set, so no negative row can leave the board stuck; `boot-hang` clears after its count of boots, and `clock-offset`, which breaks nothing, lasts until the next reboot. A normal build answers `err no-faults` and sets nothing; `hello` says which build it is (`faults 0` or `faults 1`).

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

## Revocation Trial

`revoke-test <k|p> <trial>` shows on hardware (R1) what step 2 of a disarm does, with its phase against the host's polling controlled. The trial needs 125 Hz on change, so a poll comes every 8 ms, and a held lease with nothing armed, held or owed. Its controls are F23 (the sentinel), F24 (revoked) and F22 (the probe) on the keyboard, which type nothing, and buttons 15, 16 and 14 on the pad.

1. Report k0 holds the sentinel alone; its completion C(k0) gives the host's poll phase.
2. At C(k0) + 2 ms report k1 holds the sentinel and the revoked control. At its handoff the device arms a pattern whose only item presses the probe, due 16 ms after H(k1), and records `V` with k0, k1 and the probe's due time.
3. The lease expires at C(k0) + 4 ms, 4 ms before the next poll, so the disarm goes through the expiry path while k1 is still queued: k1 is revoked (`X` with AVAILABLE set and k1's LENGTH), the pattern dropped, and the first of the two neutral reports owed releases the sentinel.
4. The host withholds `lease` and `arm` until 20 ms after the probe's due time, then arms a pattern of its own.

The verdict (`revoke::judge`) needs k1 still queued at the abort, or the trial is "not exercised". Then each failure has its own message: the revoked control seen at the listener is "revoked report arrived"; the two reports after k1 not both taken is "neutral reports not taken"; a next change after the sentinel's press other than its release, or the release seen only from the second neutral report's completion on, is "sentinel break missing after revocation" (k1 was withdrawn between polls, so the host never took it and the first neutral report must carry the PID it expects); a handoff on the endpoint after the neutral reports, or the probe seen, before the withheld window ends is "item before rearm". The crate's tests run trials on both endpoints against the simulated controller and host, whose DATA toggle handling discards a repeated PID as a duplicate and whose ACK can be lost, and show each message under its mutation: no withdrawal, no toggle restore, one neutral report, a pattern that survives the disarm.

## Clock Bounds

Device and host clocks differ in origin and rate, and round trips cannot fix the one-way delay (design.md "Test Input Device").

- **Model:** host time = a + b·(device time), assuming both clocks keep constant rates over a run (the device's crystal and the host's counter, over minutes); the verdict states the assumption. b is taken within 1000 ppm of 1.
- **Feasible set:** each ping gives s_i ≤ a + b·d_i ≤ r_i (s_i the host's send, d_i the pong's µs, r_i the host's receipt). The (a, b) satisfying all of them form a convex polygon, computed exactly: each vertex is the meeting of two constraint lines, held as i128 fractions. A device time maps to the interval of a + b·t over the vertices, rounded outwards.
- **Inconsistency:** an empty polygon fails the run as inconsistent; no observation is dropped, and the verdict names the pings whose removal alone would restore it.

Fitting b first and then intersecting the offsets that b allows is not used: when the delay asymmetry changes during a run, the fitted rate is biased (36 ppm for 3 ms out and 0.1 ms back reversing halfway through 120 s), and the offsets then exclude the truth. A unit test keeps both facts.

## Verdicts

A run's verdicts come from the files it keeps, through one contract (`test_device::contract`) that `tapestry test-device check <row> <dir>` applies. Every check reads records of what a runner, the recorder or the device observed (a record written, an answer received, a report received, an exit observed), their presence and their order within one stream; no check compares a time with a bound, and none takes a stamp written before a call as the event.

**Evidence.** One reader (`test_device::evidence`) reads every record file: UTF-8, one JSON object per line (JSON whitespace may lead it; an array is refused, though it would fill a record's fields by position), each line ending in a line feed (a carriage return or an empty line fails), no duplicate key, no number that is not an integer in its field's range (so `NaN`, infinities and `1e999` fail), and every record of a known kind with no unknown field. Each stream declares its kinds' places: its first record's kind exactly once and first, its `end` exactly once and last, and nothing after `end`. Every stream's first record names the run and the process that writes it, as the runner set them, so a stream is bound to its run and its writer from its creation. The run folder's streams:

- `run.jsonl`, the runner's: first `start` (the run id, 128 random bits in 32 hex digits; the row; the role; the device's build; the runner's process id and creation time; its cut code, `0x40000000` plus 30 random bits, the exit code it gives every process it terminates and that no process gives itself; the arguments; the set or the inner run it reserved), then `spawn` records (each child it starts: its name in the row and its process), `kill` records (a child it terminated, with every process in the job it started that child in, by its cut code), `seal` records and `exit` records (a process's end as observed once it had ended: its process, its exit code and the host counter), last `end` (the runner's exit code and its errors).
- `link.jsonl`, the link's, in the runner's process: first `open` (the run, the runner's process, the bound location and the COM port), then the commands, answers, pings, trace records, malformed cases, a refusal and a lost link, last `end`.
- `recorder.jsonl`, the recorder's: first `ready` (the run its runner gave it, and its own process id and creation time), then its `error` records, last `end`.

A stream's writer is the role's runner, or, for `recorder.jsonl`, the process the run's one `spawn` of `recorder` started; a stream whose first record names another run or another process is not the run's, and supplies nothing. A process the row names but that writes no stream (the player) is bound the same way, by its `spawn` record.

**Killed streams.** A writer cut short may leave its stream without `end` and with at most one unterminated last line, which is dropped and noted. What ended a writer is read from its exit code, never inferred from when an observer looked: a runner terminates a child, and every process in the job it started that child in, with its own cut code, so an exit that reads the observing runner's cut code, recorded after that runner's `kill` of the writer or of the child whose job held it, is that runner's cut, and any other exit is the writer ending by itself, which an observer that saw it only once it had happened cannot tell apart from an end that came earlier. A stream may stop short only when its writer was cut so. Every observer of one end must read the same exit code (the killer is the runner whose cut code it is, whoever observes it), and each observation must come after the writer's last record in every stream it wrote, since a process's end is observed once it has ended, and not before the kill it follows; records that break either leave the end ambiguous, and a stream cut short with an ambiguous end fails, as does a writer's exit rule. Any other stream without `end` or with an unterminated line fails, and so does a line after `end`. A stream that has `end` has it once and last either way, and a runner cut after it wrote its `end` counts as killed.

**Reserved runs.** Every run a row judges together is reserved by the runner before any of them starts, and `check` takes each role's run only from that reservation, never from a verdict: a set's `reservation.jsonl` (one `reserved` record: the set id, the row, the device's build, and for each role its row, a fresh run id and a folder), an outer run's `start` (its inner's role, row, run id and folder), or a single run's own `start` (the role `run`). The role's folder must hold a `start` with the reserved run id, the role, the role's row, the reserved build and the set's id (an inner run's set is its outer run's id). A complete, consistent folder from another run or set, with its own matching seals, carries another run id and fails, named.

**Seals.** Each writer, when it finalizes an artifact (a stream at its `end`, a shot once its file is written and closed, a copied settings file), records a `seal` in its run's `run.jsonl`: the run id, the role, the path inside the role's folder and the sha256. The file of a writer cut short is sealed by the runner that cut it, and only by it, after its observation of the end, with the file's role and run id (a killed run cannot seal what it wrote, so its killer seals the prefix that remains); the file of a writer nothing cut is sealed by its own role's runner. A seal in any other place, or a cutter's seal before it saw the writer end, is named. Evidence counts only through its seal: a stream other than `run.jsonl` is read once, and its records count for a record rule, an event a check reads or a check's input only when the digest of the bytes it was parsed from equals its seal; a stream with no seal, or one unlike it, supplies nothing, so a stale complete file left in the folder is not evidence. Every file a check reads must have a seal and hash to it, so a file replaced after its run, a complete image of the same build from another set included, fails, named. The one exception is the role's `run.jsonl`, which holds the seals: it is read once, its digest is that of the bytes it was parsed from, and its `start` binds it to the reservation.

**Verdict lines.** Every producer, the repository's `judge` and a private judge alike, writes `verdicts-<producer>.jsonl` in the folder `check` is given: one object per check, `{"row", "run", "check", "outcome": "pass"|"fail"|"incomplete", "detail", "inputs", "producer"}`, `run` naming the run it judges (a single run's id, or the set's), so a verdict copied from another run or set over byte-identical files does not count, `inputs` listing each file the check read by role, path and sha256, and `producer` naming the program (`name`, the file's own suffix) and its version (a commit or a sha256). A check whose evidence is missing or unreadable (no shot, a truncated image, no record) is `incomplete`, never `pass` or `fail`; a check whose evidence is present and shows the claim false is `fail`.

**Expectations are code.** Each row declares, in the repository: its roles and their rows; the exit of each process it starts (a code it ends with by itself, or killed, which needs the observing runner's cut: its cut code after its kill, observed by that runner; a role's runner is killed by another), each process other than a role's runner bound to the `spawn` record that started it, so an exit recorded under its name for another process does not stand for it; the records each stream must hold, each with its count; and the exact set of check ids, each with its producer, the outcome it must have, the files it reads by role and path, and the observed events it reads (record kinds in a role's stream). A check is held `incomplete` when an event it reads is missing or comes from a stream that is not evidence (Seals), or a stream it reads has a recorder `error`: an `error` record names the stretch of the stream it affects (from and to on the host's counter), and a check that reads that stream cannot vouch for it. A negative row's expectation also names its mutation and its trigger: a check its failing set must hold, whose failure shows that the mutation took effect.

**`check`.** `tapestry test-device check <row> <dir>` reads strictly every verdict file, the run's records and, for a set, the reservation, and compares them with the row's expectation: a role's run that is missing or not the reserved one, a stream that breaks its rules, another exit, a killed exit with no observer, a missing or extra record, a runner's error, a sealed link that recorded an error (a `lost` link, a refusal, or problems at its `end`) whatever the runner's `end` says, a missing, extra or duplicate check, another outcome, a pass or fail where the evidence is held incomplete, an input outside the declared files, and a file unlike its seal each fail, named. It prints each line it dropped from a stream cut short as a note, exits 0 only on an exact match, and its last line is `CONTRACT <row> ok` or `CONTRACT <row> mismatch <n>`. A negative row matches only by its declared failing set with every other expectation met (each exit, each record, no runner error), so a run that ended in an execution or evidence error never counts as the expected failure.

**Provenance.** Every row's first check, `provenance` (`test_device::provenance`; an expectation whose first check is another is refused): the run names a build, and every `hello` it recorded is of this protocol version, reads `faults 0` and names the build expected at that point (the run's, or the one a finished flash put on the device). Events at one host counter keep the order they were recorded in one file; a group of events at one counter that holds a flash and events of more than one file leaves their order unknown, and the check incomplete. A run with no build or no `hello` is incomplete. It checks the device's firmware only: the runner's and the player's builds are checked where they are recorded.

**Comparisons.** The `test-device` crate's `matcher` holds the timing comparisons, each failing with the first thing that breaks its rule:

- **Edges** (U1(a), U4): matching as in "Keyboard edges"; "missing edge k" or "extra edge".
- **Batches** (U2): the edges of one report share one receive time. A necessary condition only.
- **Spacing** (U1(b)): recorded intervals against handoff intervals, within one poll period, 125 µs and the listener's spread.
- **Differential** (U1(c)): d = player stamp − listener stamp for the same report, both on the host's counter; |d| ≤ 1 ms, two-sided, an empirical target recorded with its environment. It is not a latency and bounds no absolute delay.
- **Pairing** (F-S): receipt − map(C(k)) meets [0, L] with L = 4 ms (half the 125 Hz interval), and the mapping is narrower than L, so pairing report k with C(k−1) misses by one interval.
- **Latency** (F4): every receipt − map(C) interval meets [0, 20 ms].

## Host Commands

The host side is `tapestry test-device <command>` in `player`, behind `dev-tools` (Windows-only, GPL).

**One physical port.** A command binds to the device's USB location path: `DEVPKEY_Device_LocationPaths` of the composite parent, as `devices` records it. The link resolves the location to the COM port interface (`GUID_DEVINTERFACE_COMPORT`) of interface 2's device node and opens the port by that interface path, never by its COM name, which another device may take; it resolves the opened path back to its device node's composite parent's location and requires the two equal before any serial setup (line coding, DTR, queues), and requires `hello` of this protocol version (naming the expected build with `faults 0`, where one is expected) before any other command; every rediscovery repeats all of it on a new connection, and several devices are refused rather than chosen among. The host writes every command through `protocol::encode` and reads every line from the device as an answer or a trace record in its one form (Commands), anything else failing the link, the lines read with the `hello` included; a `pong` answers its ping by number, any other answer the oldest unanswered command that is not a ping.

- `devices [--out <json>]`: the composite parent `USB\VID_1209&PID_0001` and its interfaces through the configuration manager, the Raw Input handles by interface path and container id with each one's HID capabilities, the COM port of interface 2 and its `hello`, the active configuration descriptor (each endpoint's `bInterval`) from the hub's port, and the parent's location path, written to a new file (or standard output). It checks the device's shape (Verification, F1) and fails naming the first broken part, or the device's absence.
- `flash <uf2> --build <id> --port <location>`: checks every block of the UF2 (the magic words, the family flag and `rp2350-arm-s`, no not-main-flash flag, a payload of 256 bytes at a 256-byte-aligned address inside the flash, the block count, the numbering, no address written twice) before any device action, refusing the file naming the block and the rule. The flag, the payload size and the alignment are the boot ROM's block rules (RP2350 datasheet, 5.5.2 "UF2 format details"): the drive takes a file whose blocks break them, and the boot ROM writes none of those blocks. The device at the location is put into its boot ROM's drive by the 1200-baud touch on the COM port resolved from the location (no command, so a device of any protocol version takes it), unless the drive is there already. One rule for drives: the copy goes ahead only when, within 10 s, exactly one `RP2350` boot-ROM drive is present anywhere and its USB parent's location is the given one; otherwise nothing is written. Discovery fails closed: a removable volume whose identity cannot be resolved (its `INFO_UF2.TXT` unreadable, its USB parent or location unresolved) counts as a second drive, and a volume enumeration that fails before its end fails it. The drive is found from the location, never by its letter: each removable volume is named by its volume GUID path (`\\?\Volume{…}\`) and tied to a location through its disk's ancestors in the configuration manager's tree. The command creates the new file once on that path, checks through the handle (`GetFinalPathNameByHandle` with `VOLUME_NAME_GUID`) that it lies on that volume, and writes through that handle, so a letter that changes or passes to another volume, or a drive that leaves between the check and the write, sends nothing elsewhere. The file is opened write-through (`FILE_FLAG_WRITE_THROUGH`), so a write returns its count only once its bytes are written to the device, not when the system cache takes them. The copy must complete: it writes through that handle until the counts the writes return total the image's length, writing the rest again after a write that takes only part of it, and a write that fails or takes nothing before the last byte fails `flash`, naming the bytes written. A flush or close that then fails as the drive leaves (`ERROR_DEVICE_NOT_CONNECTED`, `ERROR_NO_SUCH_DEVICE`, `ERROR_DEV_NOT_EXIST`, `ERROR_DEVICE_REMOVED`, `ERROR_FILE_INVALID`, `ERROR_NOT_READY`) counts as the boot ROM's reboot, which is logged, only when every byte is known to be on the device: each was counted through the write-through handle, or the flush succeeded. Otherwise, and on any other flush or close error, `flash` fails. That shows the bytes were written to the device, not that the boot ROM took them as a whole image. Then it rediscovers the device on that location until the link resolved from it answers `hello` naming `<id>` with `faults 0`, which is `flash`'s success: the board runs that build. A reflash of the build the board already runs, which the boot ROM ignored for a reason the block check does not cover, reports success too, and the board does run that build.
- `bootsel --port <location>`: the 1200-baud touch on the device at the location, unless its drive is there already, then the drive there under the same rule.
- `trace --out <jsonl> --build <id> [--port <location>] [--ping-ms <ms>] [--no-trace] [--framing-probe]`: the passive link. It binds the device (the one present when no location is given), checks `hello` (else a `refused` record, nothing sent after `hello`, exit 1), turns the trace on (unless `--no-trace`), pings every `--ping-ms` (100 by default), and records every command, answer, pong and trace record with the host counter, in a new file and on standard output. It holds nothing and takes nothing from its standard input: a clean end of the input sends `flush`, waits for its `F`, turns the trace off and ends; a read error on the input, or its reader ending without a result, gets the same orderly end and then fails the trace naming it (first among the `end` record's problems). It exits 1 when its input failed, a ping went unanswered, a command was refused, or the trace shows a loss, a `P` or an `A`. `--framing-probe` sends each `protocol::Malformed` case instead (Commands), each followed by a ping, and requires the case's answer (`err syntax`, or `err overlong` for the 161-byte line) and the ping's `pong`, then a `hello` with no lease, `quiet 1` and the pad identified.
- `listen --out <jsonl>`: a hidden window registered for the pad and keyboards with `RIDEV_INPUTSINK`; per report the host counter, the handle, k, R, H, C and the controls; keyboard edges from the device's handle. Handles are reacquired after every arrival and removal.
- `run <run.toml>`: drives a run of the release player with a scratch `LOCALAPPDATA`: focus, lease, holds, patterns, pointer drags and clicks, client-area shots, waits on the player's output; rediscovers after any reconnect.
- `compare`: decodes the record and judges with the crate's matching and clock bounds, one JSON verdict per check.
- `check <row> <dir>`: the contract (Verdicts) for the run, set or outer run in `<dir>`, against the row's expectation compiled into the command.

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

Every row is judged by a script; an owner's look never passes one. F1's enumeration, F6, F7 and F8 are judged by their commands' own checks, each unit-tested against stand-ins, which exit nonzero naming the first broken one. Real-app rows run the release player with a scratch `LOCALAPPDATA` and `device.toml` (the device's controller: buttons 1-7 on lanes 1-7, X on the scratch, buttons 9 and 10 on START and SELECT).

| Row | Checks | Negative row |
|---|---|---|
| F0 (CI) | The image definition in the first 4 KiB; picotool names RP2350 and ARM Secure; the UF2 byte-identical to picotool's; each layout matches its descriptor; round trips; unwraps across both wraps | A build without the image definition is refused; a descriptor field's width changed fails the layout test at that field |
| F-S | 125 Hz, 500 changes: C − H in (0, 8000] µs with the minimum below 500; pairing in [0, L]; the trace agrees with the reports | `c-at-write`; pairing C(k−1) with report k |
| F1 | Enumeration (`devices`): the parent and its location; the keyboard (Generic Desktop 0x06) on interface 0 and the pad (0x05) on interface 1, each with its Raw Input handle; the pad's input report 19 bytes (18 and the report id 0), buttons 1-16, X 0-255; the COM port on interface 2 answering `hello` version 2 with the build, `faults 0`, `quiet 1` and a session token; `bInterval` matching the mode byte. In a play: one controller and one keyboard source in its record; each pattern usage's scan code | `bootsel`, then `devices`: fails naming the missing parent and its interfaces; `flash` restores them |
| F2 | 1000 Hz, 120 s continuous: `bInterval` 1, median interval 1000 ± 20 µs, gap-free across the sequence wrap, C − H ≤ 1050 µs | 125 Hz expectations; `every-other-poll` |
| F3 | 1000 → 125 → 1000 three times: everything rediscovered after the last arrival; `bInterval` and medians match the mode byte | Skipped rediscovery; `mode-byte-only` |
| F4 | 120 s of pings with the time-wrap offset: the feasible set is not empty; every receipt − map(C) meets [0, 20 ms] | Unit tests: asymmetric delays and ±200 ppm keep the truth; a pong 5 ms late is named inconsistent; pongs 50 ms early end every interval below 0 |
| F5a-c | 1200 baud; watchdog; three watchdog boots reach the drive, a healthy run between hangs does not | `baud-off`; `watchdog-8s`; a counter never cleared |
| F6 | The wire form (`trace --framing-probe`): each malformed case answered once as Commands gives, and the ping after it answered `pong` with its number; then `hello`: no lease, `quiet 1`, identified | The same probe against a local build whose framer reads with the lenient parse: fails at the first case that build takes |
| F7 | `flash` of the normal UF2 to the recorded location, first from the previous protocol version's firmware (the 1200-baud touch), then again from its own: the drive found by the location and held by its volume GUID path, the written file's handle verified on it; the device after the copy on the given location, answering `hello` version 2 with the build and `faults 0` | A UF2 with one block's magic changed: refused before any device action, the old build still answering. `--build` another id: fails after the copy naming the mismatch. `--port` a location with no device: refused with no touch and no write. A second drive and a letter change cannot be made with one board: unit tests with a stand-in volume layer cover them |
| F8 | The passive trace (`trace --ping-ms 100` for 10 s, then its input's end): the bound location and the COM port resolved from it in the first record; `hello` checked; about 100 pings, each paired; `F` with no `P`; `trace off`; a second `trace` reads another session token with the same boot half | `--build` another id: `refused`, nothing sent after `hello`, exit 1. `--port` a location with no device: refused before any port opens |
| L1-L4 | The host's process killed, a silent port, suspend and resume, a detach mid-pattern: releases within the lease plus one interval, neutral first after resume, nothing until rearmed, `hello`'s reason | `ignore-disarm`; the expiry check moved into the link's reads; no neutral before resume; `keep-armed` |
| R1 | Revocation with its phase controlled: a sentinel held in report k0, report k1 handed 2 ms after C(k0) and disarmed 2 ms later; `X` shows k1 queued; k1 never arrives; the sentinel's break arrives; a probe armed before the disarm never plays before the rearm | No revocation; no toggle restore; no rearm required |
| P1-P6 | The selection panel opened by START and SELECT held: through the controller and the keyboard, against injected input and a revert; the list under the panel stays put | Each judged by its own assertion |
| U1-U5 | Timing on real plays at both rates: every edge once and in order, spacing, the differential, batches, fast repeats, the keyboard on its own trace, recorded intervals | `drop-item`, swapped edges, a shifted player time, a changed time in a batch, deleted and inserted edges, `every-other-poll` |
| U6 | The polling-rate estimate, the low-rate warning and "not estimable" | Waits for the estimator (design.md "What Determines Timing Accuracy") |

## Room for Later Uses

- **The turntable as an encoder** needs no part: mode entries for relative or absolute axes of 8 or 16 bits, pattern items for speed and direction profiles. It does not replace real controllers' quirks (design.md "Test Input Device").
- **End-to-end latency** needs parts to buy: an analog light sensor module (an OPT101-type or TEMT6000 breakout) for the measurement square into the ADC; a 3.5 mm breakout with bias resistors, a coupling capacitor, a series resistor and clamp diodes for the audio output; a breadboard and jumpers; pin headers unless the board has them; and an opaque mount for the sensor. The firmware would sample the ADC on the second core with DMA on the same µs timer and stream detections over the link.
