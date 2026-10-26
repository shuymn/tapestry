//! HID game controllers, portable part: a report's buttons and axes to
//! packets, key config capture of buttons and turned axes, and HID Raw
//! Input blocks decoded from bytes built with the x64 layout
//! (`RAWINPUTHEADER` of 24 bytes, then `RAWHID`: `dwSizeHid` at 24,
//! `dwCount` at 28, the reports from 32).

use bms_model::TimeUs;
use eval::{ControlId, InputKind, PhysicalInputEvent, SourceId};
use input::hid::{
    AxisCap, HidDevice, HidReport, axis_control, axis_value, button_control, capture_travel,
    is_axis,
};
use input::{Capture, DeviceId, DeviceKind, SourceConfig, SourceReader, SourceSet};

const PAD: DeviceId = DeviceId(0x7a1);
const X: ControlId = ControlId(0x1_0030);
const DIAL: ControlId = ControlId(0x1_0037);

fn caps() -> [AxisCap; 2] {
    [
        AxisCap {
            control: X,
            relative: false,
            wrap: 256,
        },
        AxisCap {
            control: DIAL,
            relative: true,
            wrap: 0,
        },
    ]
}

/// A set with the controller connected and recording open, recording
/// buttons 1-4, X and the dial.
fn recording() -> (SourceSet, SourceReader) {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 4,
        max_held: 8,
        queue_capacity: 64,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(PAD, DeviceKind::Controller);
    let mut recorded: Vec<_> = (1..=4).map(button_control).collect();
    recorded.extend([X, DIAL]);
    set.set_recorded(PAD, recorded);
    set.end_batch(TimeUs(0));
    set.open_recording(TimeUs(0));
    let reader = set.take_readers().pop().unwrap();
    (set, reader)
}

fn drain(reader: &mut SourceReader) -> Vec<(u32, i32)> {
    std::iter::from_fn(|| reader.pop())
        .map(|e| match e {
            PhysicalInputEvent {
                source: SourceId(0),
                kind: InputKind::Control { control, value },
                ..
            } => (control.0, value),
            other => panic!("{other:?}"),
        })
        .collect()
}

/// One batch at `at` with `reports`.
fn batch(
    dev: &mut HidDevice,
    set: &mut SourceSet,
    capture: &Capture,
    at: i64,
    reports: &[HidReport<'_>],
) {
    assert!(set.begin_batch(TimeUs(at)));
    for r in reports {
        dev.report(r, false, capture, set);
    }
    set.end_batch(TimeUs(at + 1));
}

#[test]
fn controls_are_named_by_usage() {
    assert_eq!(button_control(1), ControlId(1));
    assert_eq!(button_control(128), ControlId(128));
    assert_eq!(axis_control(0x01, 0x30), X);
    assert!(!is_axis(button_control(u16::MAX)));
    assert!(is_axis(axis_control(0x01, 0x00)));
    // Signed axes sign-extend their field; unsigned ones do not.
    assert_eq!(axis_value(0xFF, 8, true), -1);
    assert_eq!(axis_value(0x80, 8, true), -128);
    assert_eq!(axis_value(0x7F, 8, true), 127);
    assert_eq!(axis_value(0xFF, 8, false), 255);
    assert_eq!(axis_value(0xFFFF_FFFF, 32, true), -1);
    assert_eq!(axis_value(5, 0, true), 5);
}

#[test]
fn reports_become_packets_of_changes() {
    let (mut set, mut reader) = recording();
    let capture = Capture::new();
    let mut dev = HidDevice::new(PAD, &caps());
    batch(
        &mut dev,
        &mut set,
        &capture,
        10,
        &[
            // Buttons 1 and 3 down; X first seen at 200; the dial still.
            HidReport {
                buttons: Some(&[3, 1]),
                axes: &[Some(200), Some(0)],
            },
            // The same state: nothing.
            HidReport {
                buttons: Some(&[1, 3]),
                axes: &[Some(200), Some(0)],
            },
            // A report without the buttons leaves them as they are; X
            // moves, the dial turns -2 twice.
            HidReport {
                buttons: None,
                axes: &[Some(201), Some(-2)],
            },
            HidReport {
                buttons: None,
                axes: &[None, Some(-2)],
            },
            // Button 1 up, 2 down; out-of-range usages are ignored.
            HidReport {
                buttons: Some(&[0, 2, 3, 129, 300]),
                axes: &[],
            },
        ],
    );
    assert_eq!(
        drain(&mut reader),
        [
            (1, 1),
            (3, 1),
            (0x1_0030, 200),
            (0x1_0030, 201),
            (0x1_0037, -2),
            (0x1_0037, -2),
            (1, 0),
            (2, 1),
        ]
    );
}

#[test]
fn more_axes_than_kept_are_ignored() {
    let many: Vec<AxisCap> = (0..20)
        .map(|i| AxisCap {
            control: axis_control(1, 0x30 + i),
            relative: false,
            wrap: 0,
        })
        .collect();
    let dev = HidDevice::new(PAD, &many);
    assert_eq!(dev.axes(), &many[..input::hid::MAX_AXES]);
}

#[test]
fn capture_takes_buttons_and_turned_axes() {
    let (mut set, _reader) = recording();
    set.close_recording();
    let capture = Capture::new();
    let mut dev = HidDevice::new(PAD, &caps());
    capture.arm();
    // X wobbles at rest and then turns 15 positions, one short of a
    // sixteenth of its 256: nothing is captured.
    let wobble: Vec<_> = [100, 101, 100, 99, 100, 115]
        .iter()
        .map(|&v| [Some(v), None])
        .collect();
    let reports: Vec<_> = wobble
        .iter()
        .map(|a| HidReport {
            buttons: None,
            axes: a,
        })
        .collect();
    batch(&mut dev, &mut set, &capture, 10, &reports);
    assert_eq!(capture.take(), None);
    // One more position makes 16.
    batch(
        &mut dev,
        &mut set,
        &capture,
        20,
        &[HidReport {
            buttons: None,
            axes: &[Some(116), None],
        }],
    );
    assert_eq!(capture.take(), Some((X, DeviceKind::Controller)));

    // Arming again starts the count again: 8 before and 8 after is not
    // enough; the relative dial needs 16 too.
    capture.arm();
    batch(
        &mut dev,
        &mut set,
        &capture,
        30,
        &[HidReport {
            buttons: None,
            axes: &[Some(124), Some(-8)],
        }],
    );
    capture.arm();
    batch(
        &mut dev,
        &mut set,
        &capture,
        40,
        &[HidReport {
            buttons: None,
            axes: &[Some(132), Some(-8)],
        }],
    );
    assert_eq!(capture.take(), None);
    batch(
        &mut dev,
        &mut set,
        &capture,
        50,
        &[HidReport {
            buttons: None,
            axes: &[None, Some(-8)],
        }],
    );
    assert_eq!(capture.take(), Some((DIAL, DeviceKind::Controller)));
    // A button press is captured at once, its release never.
    capture.arm();
    batch(
        &mut dev,
        &mut set,
        &capture,
        60,
        &[
            HidReport {
                buttons: Some(&[4]),
                axes: &[],
            },
            HidReport {
                buttons: Some(&[]),
                axes: &[],
            },
        ],
    );
    assert_eq!(
        capture.take(),
        Some((button_control(4), DeviceKind::Controller))
    );
    // Through the wrap: from 250, 4 is +10 the short way round (not -246,
    // which would capture), and 10 then makes 16.
    let (high, low, past) = ([Some(250), None], [Some(4), None], [Some(10), None]);
    let at = |axes| HidReport {
        buttons: None,
        axes,
    };
    batch(&mut dev, &mut set, &capture, 70, &[at(&high)]);
    capture.arm();
    batch(&mut dev, &mut set, &capture, 80, &[at(&low)]);
    assert_eq!(capture.take(), None);
    batch(&mut dev, &mut set, &capture, 90, &[at(&past)]);
    assert_eq!(capture.take(), Some((X, DeviceKind::Controller)));
}

#[test]
fn capture_travel_is_a_sixteenth_turn_at_least_16() {
    let cap = |wrap| AxisCap {
        control: X,
        relative: false,
        wrap,
    };
    assert_eq!(capture_travel(&cap(0)), 16);
    assert_eq!(capture_travel(&cap(256)), 16);
    assert_eq!(capture_travel(&cap(1024)), 64);
}

#[cfg(all(target_pointer_width = "64", target_endian = "little"))]
mod blocks {
    use input::DeviceId;
    use input::raw::decode_hid;

    /// A HID `RAWINPUT` block with `count` reports of `size` bytes and
    /// `data` after them.
    fn hid(device: u64, wparam: u64, size: u32, count: u32, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&2u32.to_le_bytes()); // RIM_TYPEHID
        b.extend_from_slice(&(32 + u32::try_from(data.len()).unwrap()).to_le_bytes());
        b.extend_from_slice(&device.to_le_bytes());
        b.extend_from_slice(&wparam.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn hid_block_splits_into_reports() {
        let data = [1, 2, 3, 4, 5, 6];
        let block = hid(0x51, 0, 3, 2, &data);
        let decoded = decode_hid(&block).unwrap();
        assert_eq!(decoded.device, DeviceId(0x51));
        assert!(!decoded.background);
        assert_eq!(
            decoded.reports().collect::<Vec<_>>(),
            [&[1, 2, 3][..], &[4, 5, 6][..]]
        );
        assert!(decode_hid(&hid(0x51, 1, 3, 2, &data)).unwrap().background);
    }

    #[test]
    fn malformed_hid_blocks_are_refused() {
        let data = [9; 6];
        // Reports beyond the block, of size 0, of a null device, a count
        // whose size overflows, a keyboard's block, and a short header.
        assert_eq!(decode_hid(&hid(0x51, 0, 3, 3, &data)), None);
        assert_eq!(decode_hid(&hid(0x51, 0, 0, 2, &data)), None);
        assert_eq!(decode_hid(&hid(0, 0, 3, 2, &data)), None);
        assert_eq!(decode_hid(&hid(0x51, 0, u32::MAX, u32::MAX, &data)), None);
        let mut keyboard = hid(0x51, 0, 3, 2, &data);
        keyboard[0] = 1;
        assert_eq!(decode_hid(&keyboard), None);
        assert_eq!(decode_hid(&hid(0x51, 0, 3, 2, &data)[..30]), None);
    }
}
