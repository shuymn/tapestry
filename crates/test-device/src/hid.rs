//! A HID report descriptor parser, enough to check that a layout in
//! [`crate::report`] puts each value where its own descriptor declares it
//! (docs/test-device.md "Reports", F0). It reads short items without report
//! ids, as the device's descriptors are written.

/// A value the descriptor declares: its usage page and usage, and its bit
/// offset and size in the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Declared {
    pub page: u16,
    pub usage: u16,
    pub bit: u32,
    pub size: u32,
}

/// Why a layout does not match its descriptor: the layout's field that
/// differs (or "descriptor" or "report length"), what the layout says and
/// what the descriptor declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    pub field: &'static str,
    pub detail: String,
}

/// The input values of `descriptor`, in report order.
///
/// # Errors
///
/// A long item, a report id, a truncated item or a Pop without a Push.
pub fn inputs(descriptor: &[u8]) -> Result<Vec<Declared>, String> {
    #[derive(Clone, Copy, Default)]
    struct Globals {
        page: u16,
        size: u32,
        count: u32,
    }
    let mut globals = Globals::default();
    let mut stack = Vec::new();
    let mut usages: Vec<(u16, u16)> = Vec::new();
    let (mut min, mut max) = (None, None);
    let mut bit = 0_u32;
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(&prefix) = descriptor.get(at) {
        if prefix == 0xFE {
            return Err(format!("long item at byte {at}"));
        }
        let len = match prefix & 3 {
            3 => 4,
            n => usize::from(n),
        };
        let data = descriptor
            .get(at + 1..at + 1 + len)
            .ok_or_else(|| format!("item at byte {at} runs past the end"))?;
        let value = data
            .iter()
            .rev()
            .fold(0_u32, |v, &b| (v << 8) | u32::from(b));
        let low = u16::try_from(value & 0xFFFF).unwrap_or(0);
        let high = u16::try_from(value >> 16).unwrap_or(0);
        match (prefix >> 2) & 3 {
            // Main.
            0 => {
                if prefix >> 4 == 0x8 {
                    for i in 0..globals.count {
                        let usage = if value & 1 != 0 {
                            None
                        } else if let (Some(lo), Some(hi)) = (min, max) {
                            let u = u32::from(lo) + i;
                            (u <= u32::from(hi))
                                .then(|| (globals.page, u16::try_from(u).unwrap_or(hi)))
                        } else {
                            usages
                                .get(usize::try_from(i).unwrap_or(usize::MAX))
                                .or(usages.last())
                                .copied()
                        };
                        if let Some((page, usage)) = usage {
                            out.push(Declared {
                                page,
                                usage,
                                bit,
                                size: globals.size,
                            });
                        }
                        bit += globals.size;
                    }
                }
                usages.clear();
                (min, max) = (None, None);
            }
            // Global.
            1 => match prefix >> 4 {
                0x0 => globals.page = low,
                0x7 => globals.size = value,
                0x8 => return Err(format!("report id at byte {at}")),
                0x9 => globals.count = value,
                0xA => stack.push(globals),
                0xB => {
                    globals = stack
                        .pop()
                        .ok_or_else(|| format!("pop without push at byte {at}"))?
                }
                _ => {}
            },
            // Local.
            2 => match prefix >> 4 {
                0x0 if len == 4 => usages.push((high, low)),
                0x0 => usages.push((globals.page, low)),
                0x1 => min = Some(low),
                0x2 => max = Some(low),
                _ => {}
            },
            _ => {}
        }
        at += 1 + len;
    }
    Ok(out)
}

/// Checks that each of `fields` lies where `descriptor` declares its usage,
/// then that the descriptor's input bits fill `report_bytes` exactly.
///
/// # Errors
///
/// The first field whose place or size differs, or the report length.
pub fn check_layout(
    descriptor: &[u8],
    fields: &[crate::report::Field],
    report_bytes: usize,
) -> Result<(), Mismatch> {
    let declared = inputs(descriptor).map_err(|detail| Mismatch {
        field: "descriptor",
        detail,
    })?;
    for f in fields {
        let found = declared
            .iter()
            .find(|d| d.page == f.page && d.usage == f.usage);
        match found {
            Some(d) if d.bit == f.bit && d.size == f.size => {}
            other => {
                return Err(Mismatch {
                    field: f.name,
                    detail: format!("layout bit {} size {}, descriptor {other:?}", f.bit, f.size),
                });
            }
        }
    }
    // The bits of the last input item, padding included.
    let bits = last_bit(descriptor);
    let want = u32::try_from(report_bytes * 8).unwrap_or(u32::MAX);
    if bits != want {
        return Err(Mismatch {
            field: "report length",
            detail: format!("descriptor {bits} bits, layout {want}"),
        });
    }
    Ok(())
}

/// Every input bit the descriptor declares, constant padding included.
fn last_bit(descriptor: &[u8]) -> u32 {
    let (mut size, mut count, mut bits) = (0_u32, 0_u32, 0_u32);
    let mut at = 0;
    while let Some(&prefix) = descriptor.get(at) {
        let len = match prefix & 3 {
            3 => 4,
            n => usize::from(n),
        };
        let value = descriptor
            .get(at + 1..at + 1 + len)
            .unwrap_or(&[])
            .iter()
            .rev()
            .fold(0_u32, |v, &b| (v << 8) | u32::from(b));
        match prefix & 0xFC {
            0x74 => size = value,
            0x94 => count = value,
            0x80 => bits += size * count,
            _ => {}
        }
        at += 1 + len;
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_ranges_and_lists_take_their_bits_in_order() {
        let d = crate::report::PAD_DESCRIPTOR;
        let got = inputs(d).unwrap();
        assert_eq!(
            got.first(),
            Some(&Declared {
                page: 9,
                usage: 1,
                bit: 0,
                size: 1
            })
        );
        assert_eq!(got.len(), 16 + 1 + 1 + 1 + 3);
        assert_eq!(
            got.last(),
            Some(&Declared {
                page: 0xFF00,
                usage: 5,
                bit: 112,
                size: 32
            })
        );
    }

    #[test]
    fn a_report_id_is_refused() {
        assert!(inputs(&[0x85, 0x01]).unwrap_err().contains("report id"));
    }
}
