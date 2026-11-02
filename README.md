# tapestry

A BMS player for Windows, written in Rust.

## Features

- **Judgment recomputable from the input log.** Every press and release is recorded with its timestamp, and judgment, score, and gauges are a pure function of `(chart, input log, rule preset)`. The same play can be re-evaluated under another rule after the fact, such as beatoraja-style or LR2-style, and every gauge of a preset is evaluated side by side regardless of the one displayed.
- **One evaluator everywhere.** Live play, result-screen re-evaluation, saved-record verification, and the headless `eval-cli` share the same evaluator code, and incremental evaluation during play matches batch evaluation.
- **Deterministic time.** All clocks (input, audio, display) are mapped to QPC. Chart, input, and judgment times are integer µs; chart timing with BPM changes and STOPs is computed with rational numbers and rounded once; gauges are fixed-point. The evaluator uses no floating point.
- **Timestamp-based judgment with a watermark.** Inputs are judged by their receive time, not by frame or poll. A miss is finalized only after the input watermark passes its deadline, so a miss is never decided ahead of an input still in flight.
- **Rules as data.** Judgment windows, gauges, scoring, and LN handling are rule presets built from closed policies. A preset has a canonical encoding and a hash, and every record stores the preset it was played with.
- **Allocation-free play processing.** Input, logic, audio mixing, and the app-side per-frame rendering work use preallocated buffers; counting allocators in tests and scripted plays check it.
- **Direct Windows I/O.** Raw Input reads keyboards and HID controllers, and event-driven WASAPI output requests the shortest engine period the device supports. Input and audio threads run under MMCSS and are connected by lock-free queues.
- **Automated architecture checks.** `cargo xtask check-arch` and `cargo xtask check-lints` guard the evaluator's dependencies and impure APIs, the Windows boundary, crate licenses, development-feature isolation, and in-play lint rules. Everything except the Windows-specific crates builds and is tested on Linux.
- **Tested by properties.** Property tests cover determinism, live/batch agreement, watermark step independence, and gauge invariants; parsers and decoders are fuzzed.

## Documentation

- [docs/design.md](docs/design.md): design
- [docs/setup.md](docs/setup.md): development environment

## License

Chart loading, rule presets, the evaluator, and the replay format (`bms-model`, `bms-parser`, `rules`, `rules-toml`, `eval`, `replay`, `eval-cli`, `xtask`) are licensed under MIT OR Apache-2.0. The rest of the game, including the player, is licensed under GPL-3.0-or-later with an additional permission. See [COPYRIGHT](COPYRIGHT).
