//! Unwrapping the pad report's 16-bit sequence number and 32-bit µs times
//! (docs/test-device.md "Wraps").

/// Unwraps a 16-bit sequence number into a monotonic count.
#[derive(Clone, Copy, Debug, Default)]
pub struct SeqUnwrap {
    last: Option<u64>,
}

/// A sequence number 32 768 or more ahead of the last one, or behind it: a
/// failure, never a wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Jump {
    pub last: u64,
    pub raw: u16,
}

impl SeqUnwrap {
    /// The full sequence number of `raw`. A repeat of the last number gives
    /// that number again.
    ///
    /// # Errors
    ///
    /// [`Jump`] when `raw` is 32 768 or more ahead of the last number.
    pub fn next(&mut self, raw: u16) -> Result<u64, Jump> {
        let full = match self.last {
            None => u64::from(raw),
            Some(last) => {
                #[expect(clippy::cast_possible_truncation, reason = "the low 16 bits")]
                let ahead = raw.wrapping_sub(last as u16);
                if ahead >= 0x8000 {
                    return Err(Jump { last, raw });
                }
                last + u64::from(ahead)
            }
        };
        self.last = Some(full);
        Ok(full)
    }
}

/// The 64-bit time whose low 32 bits are `raw`, in the 2³² µs window
/// nearest `estimate` (the round-trip mapping's estimate of the time).
#[must_use]
pub fn unwrap_time(raw: u32, estimate: u64) -> u64 {
    const SPAN: i128 = 1 << 32;
    let base = i128::from(estimate & !0xFFFF_FFFF) + i128::from(raw);
    let e = i128::from(estimate);
    let best = [base - SPAN, base, base + SPAN]
        .into_iter()
        .filter(|&c| c >= 0)
        .min_by_key(|&c| (c - e).abs())
        .unwrap_or(base);
    u64::try_from(best).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sequence_unwraps_across_its_wrap() {
        let mut u = SeqUnwrap::default();
        assert_eq!(u.next(65_534), Ok(65_534));
        assert_eq!(u.next(65_535), Ok(65_535));
        assert_eq!(u.next(0), Ok(65_536));
        assert_eq!(u.next(3), Ok(65_539));
        assert_eq!(u.next(3), Ok(65_539), "a repeat");
        assert_eq!(
            u.next(65_535),
            Err(Jump {
                last: 65_539,
                raw: 65_535
            }),
            "backwards"
        );
        assert_eq!(
            u.next(3 + 0x8000),
            Err(Jump {
                last: 65_539,
                raw: 3 + 0x8000
            })
        );
        assert_eq!(u.next(3 + 0x7FFF), Ok(65_539 + 0x7FFF));
    }

    #[test]
    fn a_time_lands_in_the_window_nearest_its_estimate() {
        let wrap = 1_u64 << 32;
        // Just after the wrap, estimated just before it.
        assert_eq!(unwrap_time(5, wrap - 10), wrap + 5);
        // Just before the wrap, estimated just after it.
        assert_eq!(unwrap_time(0xFFFF_FFF0, wrap + 3), wrap - 16);
        assert_eq!(unwrap_time(1000, 900), 1000);
        assert_eq!(unwrap_time(0xFFFF_FFFF, 0), 0xFFFF_FFFF, "never below zero");
        assert_eq!(unwrap_time(7, 3 * wrap + 9), 3 * wrap + 7);
    }
}
