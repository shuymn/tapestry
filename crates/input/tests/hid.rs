//! HID game controllers, portable part: a report's buttons and axes to
//! packets, key config capture of buttons and turned axes, and HID Raw
//! Input blocks decoded from bytes built with the x64 layout
//! (`RAWINPUTHEADER` of 24 bytes, then `RAWHID`: `dwSizeHid` at 24,
//! `dwCount` at 28, the reports from 32).

use bms_model::TimeUs;
use eval::{ControlId, InputKind, PhysicalInputEvent, SourceId};
use input::hid::{
    AxisCap, ButtonGroup, HidDevice, HidLayout, HidReport, axis_cap, axis_control, axis_value,
    button_control, capture_travel, collection_of, in_collection, is_axis, read_report,
    remove_collection,
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
            min: 0,
            max: 255,
        },
        AxisCap {
            control: DIAL,
            relative: true,
            wrap: 0,
            min: 0,
            max: 0,
        },
    ]
}

/// Collection 0 with `axes` and buttons in every report.
fn layout(axes: &[AxisCap]) -> HidLayout<'_> {
    HidLayout {
        collection: 0,
        axes,
        buttons: &[],
    }
}

/// A set with the controller and its axes connected, recording buttons
/// 1-4, X and the dial; recording closed.
fn connected() -> SourceSet {
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
    assert_eq!(set.add_axes(PAD, &caps())[..3], [true, true, false]);
    let mut recorded: Vec<_> = (1..=4).map(button_control).collect();
    recorded.extend([X, DIAL]);
    set.set_recorded(PAD, recorded);
    set.end_batch(TimeUs(0));
    set
}

/// A set with the controller connected and recording open, recording
/// buttons 1-4, X and the dial.
fn recording() -> (SourceSet, SourceReader) {
    let mut set = connected();
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
    let mut dev = HidDevice::new(PAD, &layout(&caps()));
    batch(
        &mut dev,
        &mut set,
        &capture,
        10,
        &[
            // Buttons 1 and 3 down; X first seen at 200; the dial still.
            HidReport {
                id: 0,
                buttons: Some(&[3, 1]),
                axes: &[Some(200), Some(0)],
            },
            // The same state: nothing.
            HidReport {
                id: 0,
                buttons: Some(&[1, 3]),
                axes: &[Some(200), Some(0)],
            },
            // A report without the buttons leaves them as they are; X
            // moves, the dial turns -2 twice.
            HidReport {
                id: 0,
                buttons: None,
                axes: &[Some(201), Some(-2)],
            },
            HidReport {
                id: 0,
                buttons: None,
                axes: &[None, Some(-2)],
            },
            // Button 1 up, 2 down; out-of-range usages are ignored.
            HidReport {
                id: 0,
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
            min: 0,
            max: 0,
        })
        .collect();
    let dev = HidDevice::new(PAD, &layout(&many));
    assert_eq!(dev.axes(), &many[..input::hid::MAX_AXES]);
}

#[test]
fn capture_takes_buttons_and_turned_axes() {
    let (mut set, _reader) = recording();
    set.close_recording();
    let capture = Capture::new();
    let mut dev = HidDevice::new(PAD, &layout(&caps()));
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
            id: 0,
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
            id: 0,
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
            id: 0,
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
            id: 0,
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
            id: 0,
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
                id: 0,
                buttons: Some(&[4]),
                axes: &[],
            },
            HidReport {
                id: 0,
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
        id: 0,
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
        min: 0,
        max: 0,
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

#[test]
fn absolute_axis_at_zero_is_in_the_snapshot_across_retry() {
    let mut set = connected();
    let capture = Capture::new();
    let mut dev = HidDevice::new(PAD, &layout(&caps()));
    // The turntable rests at 0; the dial reported a movement once.
    batch(
        &mut dev,
        &mut set,
        &capture,
        10,
        &[HidReport {
            id: 0,
            buttons: Some(&[2]),
            axes: &[Some(0), Some(3)],
        }],
    );
    let first = set.open_recording(TimeUs(20));
    set.close_recording();
    // The retry's snapshot still has it, 0 included; a relative axis has
    // no position.
    let start = set.open_recording(TimeUs(30));
    assert_eq!(first.held, start.held);
    assert_eq!(
        start.held,
        [(SourceId(0), button_control(2), 1), (SourceId(0), X, 0)]
    );
    // The evaluator's conversion starts from it: the first turn presses.
    let profile = eval::DeviceProfile {
        sources: vec![eval::SourceProfile {
            axes: vec![eval::AxisBinding {
                control: X,
                lane: bms_model::Lane::SCRATCH,
                scratch: eval::ScratchSettings::DEFAULT,
            }],
            ..eval::SourceProfile::default()
        }],
    };
    let mut c = eval::ConversionState::init(&profile, &start).unwrap();
    let turn = PhysicalInputEvent {
        received: TimeUs(40),
        source: SourceId(0),
        kind: InputKind::Control {
            control: X,
            value: 2,
        },
    };
    assert_eq!(
        c.apply(&turn, turn.received).as_slice(),
        [eval::LaneEdge::Press(bms_model::Lane::SCRATCH)]
    );
}

#[test]
fn unchanged_background_report_trips_the_gate() {
    let (mut set, mut reader) = recording();
    let capture = Capture::new();
    let mut dev = HidDevice::new(PAD, &layout(&caps()));
    let still = HidReport {
        id: 0,
        buttons: Some(&[]),
        axes: &[None, None],
    };
    let press = HidReport {
        id: 0,
        buttons: Some(&[1]),
        axes: &[None, None],
    };
    assert!(set.begin_batch(TimeUs(10)));
    // Nothing changed in the background report, so it makes no packet.
    dev.report(&still, true, &capture, &mut set);
    dev.report(&press, false, &capture, &mut set);
    set.end_batch(TimeUs(11));
    assert!(set.gate().is_tripped());
    assert_eq!(drain(&mut reader), []);
    // The physical state followed: button 1 is held, so the same report
    // again changes nothing.
    set.gate_mut().reset();
    set.open_recording(TimeUs(20));
    let mut reader = set.take_readers().pop().unwrap();
    batch(&mut dev, &mut set, &capture, 30, &[press]);
    assert_eq!(drain(&mut reader), []);
}

#[test]
fn each_report_id_changes_only_its_own_buttons() {
    let (mut set, mut reader) = recording();
    let capture = Capture::new();
    let groups = [
        ButtonGroup {
            report_id: 1,
            buttons: 0b0011,
        },
        ButtonGroup {
            report_id: 2,
            buttons: 0b1100,
        },
    ];
    let mut dev = HidDevice::new(
        PAD,
        &HidLayout {
            collection: 0,
            axes: &[],
            buttons: &groups,
        },
    );
    let r = |id, buttons| HidReport {
        id,
        buttons: Some(buttons),
        axes: &[],
    };
    batch(
        &mut dev,
        &mut set,
        &capture,
        10,
        &[
            r(1, &[1]),
            // Report 2 carries buttons 3 and 4 only: button 1 stays down,
            // and a button it does not carry is ignored.
            r(2, &[3, 2]),
            r(1, &[]),
            r(2, &[3]),
            // An id with no buttons changes none.
            r(7, &[4]),
        ],
    );
    assert_eq!(drain(&mut reader), [(1, 1), (3, 1), (1, 0)]);
}

#[test]
fn collections_of_one_controller_are_one_source() {
    let mut set = connected();
    let x2 = in_collection(X, 2);
    let b2 = in_collection(button_control(1), 2);
    set.set_recorded(PAD, vec![button_control(1), b2, x2]);
    set.open_recording(TimeUs(5));
    let mut reader = set.take_readers().pop().unwrap();
    let capture = Capture::new();
    let mut first = HidDevice::new(PAD, &layout(&[]));
    let second_axes = [AxisCap {
        control: x2,
        relative: false,
        wrap: 256,
        min: 0,
        max: 255,
    }];
    let mut second = HidDevice::new(
        PAD,
        &HidLayout {
            collection: 2,
            axes: &second_axes,
            buttons: &[],
        },
    );
    assert!(set.begin_batch(TimeUs(10)));
    let button = HidReport {
        id: 0,
        buttons: Some(&[1]),
        axes: &[],
    };
    first.report(&button, false, &capture, &mut set);
    second.report(
        &HidReport {
            axes: &[Some(40)],
            ..button
        },
        false,
        &capture,
        &mut set,
    );
    set.end_batch(TimeUs(11));
    assert_eq!(
        drain(&mut reader),
        [(1, 1), (0x0200_0001, 1), (0x0201_0030, 40)]
    );
    assert_eq!(set.participants().count(), 1);
}

#[test]
fn controls_carry_their_collection() {
    assert_eq!(in_collection(X, 2), ControlId(0x0201_0030));
    assert!(is_axis(in_collection(X, 255)));
    assert!(!is_axis(in_collection(button_control(5), 2)));
    assert_eq!(in_collection(in_collection(X, 3), 0), X);
    let path = |s: &str| s.encode_utf16().collect::<Vec<_>>();
    assert_eq!(collection_of(&path("hid#vid_0&pid_0&Col02#7&0&0001")), 2);
    assert_eq!(collection_of(&path("HID#VID_0&PID_0&COL0A#x")), 10);
    assert_eq!(collection_of(&path("HID#VID_0&PID_0#x")), 0);
    assert_eq!(collection_of(&path("&colZ1")), 0);
    assert_eq!(collection_of(&path("&col")), 0);
}

#[test]
fn axis_cap_takes_the_declared_range_of_any_shape() {
    let cap = |absolute, min, max, bits| axis_cap(X, absolute, min, max, bits);
    let absolute = |wrap, min, max| {
        Some(AxisCap {
            control: X,
            relative: false,
            wrap,
            min,
            max,
        })
    };
    // Signed and unsigned 8 bits.
    assert_eq!(cap(true, -128, 127, 8), absolute(256, -128, 127));
    assert_eq!(cap(true, 0, 255, 8), absolute(256, 0, 255));
    // A narrower declaration is taken as declared, whatever the field.
    assert_eq!(cap(true, -127, 127, 8), absolute(255, -127, 127));
    assert_eq!(cap(true, 0, 254, 8), absolute(255, 0, 254));
    // Unsigned 10 and 12 bits, as encoders report them in 16-bit fields.
    assert_eq!(cap(true, 0, 1023, 16), absolute(1024, 0, 1023));
    assert_eq!(cap(true, 0, 4095, 12), absolute(4096, 0, 4095));
    // Signed 16 bits, and unsigned 16 bits declaring its maximum as -1.
    assert_eq!(
        cap(true, -32_768, 32_767, 16),
        absolute(65_536, -32_768, 32_767)
    );
    assert_eq!(cap(true, 0, -1, 16), absolute(65_536, 0, 65_535));
    // A relative axis has no range.
    assert_eq!(
        cap(false, -127, 127, 8),
        Some(AxisCap {
            control: X,
            relative: true,
            wrap: 0,
            min: 0,
            max: 0,
        })
    );
    // A full 32-bit range does not fit in a u32; an empty one is no range.
    assert_eq!(cap(true, i32::MIN, i32::MAX, 32), None);
    assert_eq!(cap(true, 0, -1, 32), None);
    assert_eq!(cap(true, 5, 4, 0), None);
}

#[test]
fn values_outside_the_declared_range_are_recorded_and_counted() {
    // A descriptor declaring -127..=127 for an axis that also reports
    // -128: the value is recorded as reported, and counted.
    let (mut set, mut reader) = recording();
    let capture = Capture::new();
    let narrow = [axis_cap(X, true, -127, 127, 8).unwrap()];
    let mut dev = HidDevice::new(PAD, &layout(&narrow));
    let at = |v| [Some(v)];
    let (a, b, c) = (at(127), at(-128), at(-127));
    let r = |axes| HidReport {
        id: 0,
        buttons: None,
        axes,
    };
    batch(
        &mut dev,
        &mut set,
        &capture,
        10,
        &[r(&a), r(&b), r(&c), r(&b)],
    );
    assert_eq!(
        drain(&mut reader),
        [
            (0x1_0030, 127),
            (0x1_0030, -128),
            (0x1_0030, -127),
            (0x1_0030, -128)
        ]
    );
    assert_eq!(set.axis_out_of_range().sum::<u64>(), 2);
    // A relative axis is never out of range.
    let dial = [axis_cap(DIAL, false, 0, 0, 8).unwrap()];
    let mut dev = HidDevice::new(PAD, &layout(&dial));
    batch(&mut dev, &mut set, &capture, 20, &[r(&at(-90))]);
    assert_eq!(set.axis_out_of_range().sum::<u64>(), 2);
    // A new recording starts the count again.
    set.close_recording();
    set.open_recording(TimeUs(30));
    assert_eq!(set.axis_out_of_range().sum::<u64>(), 0);
}

#[test]
fn signed_8_bit_reports_sign_extend_into_their_range() {
    // The field's raw values read as the axis reports them.
    let cap = axis_cap(X, true, -128, 127, 8).unwrap();
    for (raw, value) in [(0x7F, 127), (0x80, -128), (0xFF, -1), (0, 0)] {
        let v = axis_value(raw, 8, true);
        assert_eq!(v, value);
        assert!((cap.min..=cap.max).contains(&v));
    }
}

#[test]
fn reports_larger_than_the_buffer_mark_the_recording_incomplete() {
    let (mut set, _reader) = recording();
    let mut buf = [0_u8; 4];
    assert_eq!(
        read_report(&[1, 2, 3, 4], &mut buf, &mut set).map(|r| r.to_vec()),
        Some(vec![1, 2, 3, 4])
    );
    assert!(!set.overflowed());
    assert_eq!(read_report(&[0; 5], &mut buf, &mut set), None);
    assert!(set.overflowed());
}

#[test]
fn a_controller_without_room_marks_the_next_recordings() {
    let mut set = connected();
    let unread = DeviceId(0xdead);
    // Found twice, as the device list and its arrival both report it: one
    // device missing.
    set.connect_unread(unread, DeviceKind::Controller);
    set.connect_unread(unread, DeviceKind::Controller);
    set.open_recording(TimeUs(5));
    assert!(set.source_limited());
    assert_eq!(set.participants().count(), 1);
    // Its removal clears it; a second removal changes nothing more.
    set.close_recording();
    set.disconnect(unread);
    set.open_recording(TimeUs(6));
    assert!(!set.source_limited());
    set.close_recording();
    set.connect_unread(DeviceId(0xbeef), DeviceKind::Controller);
    set.disconnect(unread);
    set.open_recording(TimeUs(7));
    assert!(set.source_limited(), "another unread device is still there");
}

#[test]
fn removing_one_collection_disconnects_the_controller_until_the_next_play() {
    let (mut set, mut reader) = recording();
    let capture = Capture::new();
    let b2 = in_collection(button_control(1), 2);
    let mut first = HidDevice::new(PAD, &layout(&caps()));
    let second = HidDevice::new(
        PAD,
        &HidLayout {
            collection: 2,
            axes: &[],
            buttons: &[],
        },
    );
    // Button 2 of the first collection is held when that collection goes.
    batch(
        &mut first,
        &mut set,
        &capture,
        10,
        &[HidReport {
            id: 0,
            buttons: Some(&[2]),
            axes: &[Some(7), None],
        }],
    );
    assert!(set.begin_batch(TimeUs(20)));
    remove_collection(PAD, [&second], &mut set);
    set.end_batch(TimeUs(21));
    // The source is disconnected, which releases everything it held, and
    // the controller arrives again with the collection that remains.
    let events: Vec<_> = std::iter::from_fn(|| reader.pop()).collect();
    assert_eq!(events.last().map(|e| e.kind), Some(InputKind::Disconnected));
    assert_eq!(set.arrivals().len(), 1);
    // At the next play it takes part, with nothing of the removed
    // collection: no held button, no axis.
    set.close_recording();
    set.set_recorded(PAD, vec![button_control(2), b2, X]);
    let start = set.open_recording(TimeUs(30));
    assert_eq!(set.participants().count(), 1);
    assert!(start.held.is_empty(), "{:?}", start.held);
    assert_eq!(set.axes(PAD), []);
    // The last collection's removal disconnects it for good.
    set.close_recording();
    remove_collection(PAD, [], &mut set);
    set.open_recording(TimeUs(40));
    assert_eq!(set.participants().count(), 0);
}

#[test]
fn axes_past_the_table_are_not_kept() {
    let mut set = connected();
    // X and the dial are kept; 14 more fill the table of 16, and a 17th
    // axis, in another collection, does not fit.
    let more: Vec<AxisCap> = (0..15)
        .map(|i| AxisCap {
            control: in_collection(
                axis_control(1, 0x30 + i % 9),
                2 + u8::try_from(i / 9).unwrap(),
            ),
            relative: false,
            wrap: 256,
            min: 0,
            max: 255,
        })
        .collect();
    let kept = set.add_axes(PAD, &more);
    assert_eq!(kept[..15].iter().filter(|&&k| k).count(), 14);
    assert!(!kept[14]);
    assert_eq!(set.axes(PAD).len(), 16);
    assert!(!set.axes(PAD).contains(&more[14]));
    // An axis already kept is kept again; a device not connected keeps
    // none.
    assert!(set.add_axes(PAD, &caps())[0]);
    assert_eq!(set.add_axes(DeviceId(1), &caps()), [false; 16]);
}
