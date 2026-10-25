//! Raw Input packets decoded from bytes built here with the documented
//! x64 layout: `RAWINPUTHEADER` is `dwType` at 0, `dwSize` at 4, `hDevice`
//! at 8 and `wParam` at 16 (24 bytes), then `RAWKEYBOARD` with `MakeCode`
//! at 24, `Flags` at 26, `Reserved` at 28, `VKey` at 30, `Message` at 32
//! and `ExtraInformation` at 36 (40 bytes in all).

#![cfg(all(target_pointer_width = "64", target_endian = "little"))]

use bms_model::TimeUs;
use eval::{ControlId, InputKind, PhysicalInputEvent, SourceId};
use input::raw::{blocks, decode_keyboard};
use input::{Admit, DeviceId, DeviceKind, Packet, SourceConfig, SourceSet};

const BREAK: u16 = 1;
const E0: u16 = 2;
const E1: u16 = 4;
/// `GET_RAWINPUT_CODE_WPARAM` values.
const RIM_INPUT: u64 = 0;
const RIM_INPUTSINK: u64 = 1;

/// A keyboard `RAWINPUT` block.
fn keyboard(device: u64, wparam: u64, make: u16, flags: u16, vkey: u16) -> Vec<u8> {
    let message: u32 = if flags & BREAK == 0 { 0x100 } else { 0x101 };
    let mut b = Vec::with_capacity(40);
    b.extend_from_slice(&1u32.to_le_bytes()); // RIM_TYPEKEYBOARD
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&device.to_le_bytes());
    b.extend_from_slice(&wparam.to_le_bytes());
    b.extend_from_slice(&make.to_le_bytes());
    b.extend_from_slice(&flags.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&vkey.to_le_bytes());
    b.extend_from_slice(&message.to_le_bytes());
    b.extend_from_slice(&0x5EED_u32.to_le_bytes());
    assert_eq!(b.len(), 40);
    b
}

fn decoded(device: u64, control: u32, value: i32) -> Option<Packet> {
    Some(Packet {
        device: DeviceId(device),
        control: ControlId(control),
        value,
        background: false,
    })
}

#[test]
fn keyboard_packet_decodes_to_scan_code_edge() {
    let cases = [
        // A: press.
        (
            keyboard(0x11A0, RIM_INPUT, 0x1E, 0, 0x41),
            decoded(0x11A0, 0x1E, 1),
        ),
        // Left Ctrl and right Ctrl differ only by E0.
        (
            keyboard(0x22B0, RIM_INPUT, 0x1D, 0, 0x11),
            decoded(0x22B0, 0x1D, 1),
        ),
        (
            keyboard(0x33C0, RIM_INPUT, 0x1D, E0 | BREAK, 0x11),
            decoded(0x33C0, 0xE01D, 0),
        ),
        // Pause: E1 1D, then a 45 with no virtual key that is not a key.
        (
            keyboard(0x44D0, RIM_INPUT, 0x1D, E1, 0x13),
            decoded(0x44D0, 0xE11D, 1),
        ),
        (keyboard(0x44D0, RIM_INPUT, 0x45, 0, 0xFF), None),
        (
            keyboard(0x55E0, RIM_INPUT, 0x1D, E1 | BREAK, 0x13),
            decoded(0x55E0, 0xE11D, 0),
        ),
        (keyboard(0x55E0, RIM_INPUT, 0x45, BREAK, 0xFF), None),
        // NumLock shares the make code 45 and keeps its virtual key.
        (
            keyboard(0x66F0, RIM_INPUT, 0x45, 0, 0x90),
            decoded(0x66F0, 0x45, 1),
        ),
        // Print Screen: the fake E0 2A shift around E0 37.
        (keyboard(0x7701, RIM_INPUT, 0x2A, E0, 0xFF), None),
        (
            keyboard(0x7701, RIM_INPUT, 0x37, E0, 0x2C),
            decoded(0x7701, 0xE037, 1),
        ),
        (
            keyboard(0x8802, RIM_INPUT, 0x37, E0 | BREAK, 0x2C),
            decoded(0x8802, 0xE037, 0),
        ),
        (keyboard(0x8802, RIM_INPUT, 0x2A, E0 | BREAK, 0xFF), None),
        // The fake right shift; the real shifts have no prefix.
        (keyboard(0x9903, RIM_INPUT, 0x36, E0, 0xFF), None),
        (
            keyboard(0xAA04, RIM_INPUT, 0x2A, 0, 0x10),
            decoded(0xAA04, 0x2A, 1),
        ),
        (
            keyboard(0xBB05, RIM_INPUT, 0x36, BREAK, 0x10),
            decoded(0xBB05, 0x36, 0),
        ),
    ];
    let recorded = cases.iter().filter(|(_, want)| want.is_some()).count();
    assert_eq!(recorded, 10);
    for (i, (block, want)) in cases.iter().enumerate() {
        assert_eq!(decode_keyboard(block), *want, "case {i}");
    }
}

#[test]
fn background_marked_packet_is_rejected() {
    const HANDLE: u64 = 0x0001_0047;
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 3,
        max_sources: 2,
        max_held: 5,
        queue_capacity: 64,
        max_arrivals: 4,
    })
    .unwrap();
    set.connect(DeviceId(HANDLE), DeviceKind::Keyboard);
    set.set_recorded(DeviceId(HANDLE), vec![ControlId(0x1E), ControlId(0x1F)]);
    set.open_recording(TimeUs(0));
    let mut readers = set.take_readers();

    let foreground = decode_keyboard(&keyboard(HANDLE, RIM_INPUT, 0x1E, 0, 0x41)).unwrap();
    let background = decode_keyboard(&keyboard(HANDLE, RIM_INPUTSINK, 0x1F, 0, 0x53)).unwrap();
    assert!(set.begin_batch(TimeUs(700)));
    assert_eq!(set.packet(&foreground), Admit::Record);
    assert_eq!(set.packet(&background), Admit::Reject);
    set.end_batch(TimeUs(702));

    assert!(set.gate().is_tripped());
    let stream: Vec<_> = std::iter::from_fn(|| readers[0].pop()).collect();
    assert_eq!(
        stream,
        [PhysicalInputEvent {
            received: TimeUs(700),
            source: SourceId(0),
            kind: InputKind::Control {
                control: ControlId(0x1E),
                value: 1,
            },
        }]
    );
}

#[test]
fn unattributable_or_malformed_packets_are_not_decoded() {
    let valid = keyboard(0xC0DE, RIM_INPUT, 0x10, 0, 0x51);
    assert!(decode_keyboard(&valid).is_some());
    // A null device handle: input injected by software.
    assert_eq!(
        decode_keyboard(&keyboard(0, RIM_INPUT, 0x10, 0, 0x51)),
        None
    );
    // Overrun, empty and out-of-range make codes.
    for make in [0xFF, 0x00, 0x100] {
        assert_eq!(
            decode_keyboard(&keyboard(0xC0DE, RIM_INPUT, make, 0, 0x51)),
            None
        );
    }
    // A mouse block, and a block cut short.
    let mut mouse = valid.clone();
    mouse[0] = 0;
    assert_eq!(decode_keyboard(&mouse), None);
    assert_eq!(decode_keyboard(&valid[..39]), None);
}

#[test]
fn background_flag_reads_only_the_input_code() {
    // GET_RAWINPUT_CODE_WPARAM keeps the low byte.
    let sink = decode_keyboard(&keyboard(0xD1, 0x7700 | RIM_INPUTSINK, 0x11, 0, 0x57)).unwrap();
    assert!(sink.background);
    let input = decode_keyboard(&keyboard(0xD2, 0x7700 | RIM_INPUT, 0x12, 0, 0x45)).unwrap();
    assert!(!input.background);
}

/// A block of another device type with its own size.
fn other(size: u32) -> Vec<u8> {
    let mut b = vec![0xAB; size as usize];
    b[..4].copy_from_slice(&2u32.to_le_bytes()); // RIM_TYPEHID
    b[4..8].copy_from_slice(&size.to_le_bytes());
    b
}

#[test]
fn buffer_blocks_start_at_pointer_alignment() {
    // A 44-byte block is padded to 48; the keyboard block follows there.
    let mut buffer = other(44);
    buffer.resize(48, 0);
    buffer.extend(keyboard(0xE7, RIM_INPUT, 0x2C, 0, 0x5A));
    buffer.extend(keyboard(0xF8, RIM_INPUT, 0x2D, BREAK, 0x58));
    let walked: Vec<_> = blocks(&buffer, 3).collect();
    assert_eq!(walked.len(), 3);
    assert_eq!(walked[0].len(), 44);
    assert_eq!(decode_keyboard(walked[0]), None);
    assert_eq!(decode_keyboard(walked[1]), decoded(0xE7, 0x2C, 1));
    assert_eq!(decode_keyboard(walked[2]), decoded(0xF8, 0x2D, 0));
    // The count bounds the walk.
    assert_eq!(blocks(&buffer, 2).count(), 2);
}

#[test]
fn buffer_walk_stops_at_a_bad_size() {
    let first = keyboard(0x1F1, RIM_INPUT, 0x02, 0, 0x31);
    // Shorter than a header.
    let mut short = first.clone();
    short.extend(other(24));
    short[44..48].copy_from_slice(&23u32.to_le_bytes());
    assert_eq!(blocks(&short, 2).count(), 1);
    // Past the end of the buffer.
    let mut long = first.clone();
    long.extend(other(24));
    long[44..48].copy_from_slice(&25u32.to_le_bytes());
    assert_eq!(blocks(&long, 2).count(), 1);
    // Exactly a header fits.
    let mut exact = first;
    exact.extend(other(24));
    assert_eq!(blocks(&exact, 2).count(), 2);
}
