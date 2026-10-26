//! Raw Input packets as bytes: walking a buffer of `RAWINPUT` blocks and
//! decoding a keyboard block into a [`Packet`]. Pure functions over bytes,
//! so they are tested on every platform; only the OS calls that fill the
//! buffers are Windows code.
//!
//! The layout is that of the build target: `RAWINPUTHEADER` is two `u32`
//! fields (`dwType`, `dwSize`) followed by two pointer-sized fields
//! (`hDevice`, `wParam`), and `RAWKEYBOARD` follows it.

use eval::ControlId;

use crate::gate::{DeviceId, Packet};

const PTR: usize = size_of::<usize>();

/// `size_of::<RAWINPUTHEADER>()` on the build target.
pub const HEADER_SIZE: usize = 8 + 2 * PTR;

/// `size_of::<RAWKEYBOARD>()`.
pub const KEYBOARD_SIZE: usize = 16;

/// `RIM_TYPEKEYBOARD`.
pub const RIM_TYPEKEYBOARD: u32 = 1;
/// `RIM_INPUTSINK`: the input arrived while the process was in the
/// background.
pub const RIM_INPUTSINK: usize = 1;
/// `RI_KEY_BREAK`: the key was released.
pub const RI_KEY_BREAK: u16 = 1;
/// `RI_KEY_E0`: the scan code has the `E0` prefix.
pub const RI_KEY_E0: u16 = 2;
/// `RI_KEY_E1`: the scan code has the `E1` prefix.
pub const RI_KEY_E1: u16 = 4;
/// `KEYBOARD_OVERRUN_MAKE_CODE`: the keyboard reported too many keys.
const OVERRUN_MAKE_CODE: u16 = 0xFF;
/// The virtual key Raw Input gives a fake key of an escape sequence.
const VK_FAKE: u16 = 0xFF;

/// The control of a key: its set 1 make code, with `0xE000` added for an
/// `E0` prefix and `0xE100` for an `E1` prefix (Pause is `0xE11D`). The
/// value does not depend on the keyboard layout, so bindings by scan code
/// are shared by every keyboard.
#[must_use]
pub const fn key_control(make_code: u8, flags: u16) -> ControlId {
    let prefix = if flags & RI_KEY_E1 != 0 {
        0xE100
    } else if flags & RI_KEY_E0 != 0 {
        0xE000
    } else {
        0
    };
    ControlId(prefix | make_code as u32)
}

/// The scan code a window's keyboard message carries for the key with
/// Raw Input control `control` ([`key_control`]): the same, `E0` keys
/// included, except for two. The window names Pause `0x45` (the make code
/// that follows its `E1 1D`, where Raw Input gives `0xE11D`) and NumLock
/// `0xE045` (Raw Input: `0x45`). A window turns this into its key (winit's
/// `PhysicalKey::from_scancode`), so a key bound through Raw Input is the
/// same key in the window.
#[must_use]
pub const fn window_scan_code(control: ControlId) -> u32 {
    match control.0 {
        0xE11D => 0x45,
        0x45 => 0xE045,
        c => c,
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let b = bytes.get(at..at + 2)?;
    Some(u16::from_ne_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
}

fn usize_at(bytes: &[u8], at: usize) -> Option<usize> {
    let b = bytes.get(at..at + PTR)?;
    let mut word = [0; PTR];
    word.copy_from_slice(b);
    Some(usize::from_ne_bytes(word))
}

/// Decodes one `RAWINPUT` block of a keyboard into a key packet.
///
/// Returns `None` for anything that is not one key of an attributable
/// keyboard: a block that is not a keyboard's or is too short, a null
/// device handle (input injected by software, which no device owns), an
/// overrun or empty make code, and the fake keys of escape sequences. Those
/// are the `E0 2A` and `E0 36` shifts Print Screen and the navigation keys
/// wrap themselves in, and the `45` that follows Pause's `E1 1D` with no
/// virtual key; NumLock, whose make code is also `45`, keeps its virtual
/// key. The packet's `background` is the header's `RIM_INPUTSINK`.
#[must_use]
pub fn decode_keyboard(block: &[u8]) -> Option<Packet> {
    if block.len() < HEADER_SIZE + KEYBOARD_SIZE || u32_at(block, 0)? != RIM_TYPEKEYBOARD {
        return None;
    }
    let device = usize_at(block, 8)?;
    let wparam = usize_at(block, 8 + PTR)?;
    let make = u16_at(block, HEADER_SIZE)?;
    let flags = u16_at(block, HEADER_SIZE + 2)?;
    let vkey = u16_at(block, HEADER_SIZE + 6)?;
    if device == 0 || make == 0 || make >= OVERRUN_MAKE_CODE {
        return None;
    }
    let e0 = flags & RI_KEY_E0 != 0;
    let fake_shift = e0 && (make == 0x2A || make == 0x36);
    let pause_tail = flags & (RI_KEY_E0 | RI_KEY_E1) == 0 && make == 0x45 && vkey == VK_FAKE;
    if fake_shift || pause_tail {
        return None;
    }
    // `make < 0xFF` was checked above.
    let make = u8::try_from(make).ok()?;
    Some(Packet {
        device: DeviceId(device as u64),
        control: key_control(make, flags),
        value: i32::from(flags & RI_KEY_BREAK == 0),
        background: wparam & 0xFF == RIM_INPUTSINK,
    })
}

/// The `RAWINPUT` blocks `GetRawInputBuffer` wrote into `bytes`: `count`
/// blocks, each `dwSize` long and starting at a pointer-size boundary
/// (`NEXTRAWINPUTBLOCK`). The walk stops early at a block whose size does
/// not fit in `bytes` or is shorter than a header.
#[must_use]
pub const fn blocks(bytes: &[u8], count: usize) -> Blocks<'_> {
    Blocks {
        bytes,
        at: 0,
        left: count,
    }
}

/// The iterator of [`blocks`].
#[derive(Clone, Debug)]
pub struct Blocks<'a> {
    bytes: &'a [u8],
    at: usize,
    left: usize,
}

impl<'a> Iterator for Blocks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.left == 0 {
            return None;
        }
        let size = u32_at(self.bytes, self.at + 4)? as usize;
        let end = self.at.checked_add(size)?;
        if size < HEADER_SIZE || end > self.bytes.len() {
            self.left = 0;
            return None;
        }
        let block = &self.bytes[self.at..end];
        self.left -= 1;
        self.at = end.next_multiple_of(PTR);
        Some(block)
    }
}
