//! The practice speed (design.md "Practice Speed Changes"), shared by the
//! loader, which maps chart time to real time, the records, and the sound
//! bank.

/// A practice speed on the ladder `k/20`, k = 10 to 30 (50 % to 150 %): a
/// chart time `t` plays at the real time `t × 20/k`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Speed(u8);

impl Default for Speed {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl Speed {
    /// 1×: real time is chart time.
    pub const NORMAL: Self = Self(20);
    /// The slowest step, 50 %.
    pub const SLOWEST: Self = Self(10);
    /// The fastest step, 150 %.
    pub const FASTEST: Self = Self(30);

    /// The speed `k/20`, if k is on the ladder.
    #[must_use]
    pub fn from_twentieths(k: u32) -> Option<Self> {
        u8::try_from(k)
            .ok()
            .filter(|k| (Self::SLOWEST.0..=Self::FASTEST.0).contains(k))
            .map(Self)
    }

    /// Every step from 50 % to 150 %.
    pub fn ladder() -> impl Iterator<Item = Self> {
        (Self::SLOWEST.0..=Self::FASTEST.0).map(Self)
    }

    /// k of `k/20`.
    #[must_use]
    pub fn twentieths(self) -> u32 {
        u32::from(self.0)
    }

    /// The speed in lowest terms, chart time to real time: `k/g : 20/g`
    /// with `g = gcd(k, 20)` (1:2 at 50 %, 13:20 at 65 %, 29:20 at 145 %).
    #[must_use]
    pub fn ratio(self) -> (u32, u32) {
        let k = u32::from(self.0);
        let g = gcd(k, 20);
        (k / g, 20 / g)
    }

    /// The real length of `n` units of chart time: `floor(n × 20/k)`.
    #[must_use]
    pub fn stretch(self, n: u64) -> u64 {
        let n = u128::from(n) * 20 / u128::from(self.0);
        u64::try_from(n).unwrap_or(u64::MAX)
    }
}

const fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::Speed;

    #[test]
    fn the_ladder_is_k_over_20_from_10_to_30() {
        let ks: Vec<u32> = Speed::ladder().map(Speed::twentieths).collect();
        assert_eq!(ks, (10..=30).collect::<Vec<_>>());
        assert_eq!(Speed::from_twentieths(9), None);
        assert_eq!(Speed::from_twentieths(31), None);
        assert_eq!(Speed::from_twentieths(276), None);
        assert_eq!(Speed::from_twentieths(20), Some(Speed::NORMAL));
        assert_eq!(Speed::default(), Speed::NORMAL);
    }

    #[test]
    fn ratio_is_in_lowest_terms_and_stretch_floors() {
        let at = |k| Speed::from_twentieths(k).unwrap();
        assert_eq!(at(10).ratio(), (1, 2));
        assert_eq!(at(13).ratio(), (13, 20));
        assert_eq!(at(29).ratio(), (29, 20));
        assert_eq!(Speed::NORMAL.ratio(), (1, 1));
        assert_eq!(at(13).stretch(1_089), 1_675);
        assert_eq!(at(30).stretch(u64::MAX), u64::MAX / 3 * 2);
        assert_eq!(at(10).stretch(u64::MAX), u64::MAX);
    }
}
