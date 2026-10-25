//! Primitive values of the body: LEB128 varints, zigzag for signed values,
//! fixed-width little-endian section lengths, and length-prefixed strings.

use crate::DecodeError;

/// Appends values to a buffer.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn uint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(byte);
                return;
            }
            self.0.push(byte | 0x80);
        }
    }

    pub fn int(&mut self, v: i64) {
        self.uint(zigzag(v));
    }

    pub fn len(&mut self, n: usize) {
        self.uint(n as u64);
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    pub fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.raw(bytes);
    }

    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

pub const fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

pub const fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

/// Reads values from a slice; every read checks the bytes left.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub const fn remaining(&self) -> usize {
        self.buf.len()
    }

    /// Fails unless every byte was read.
    pub const fn finish(&self) -> Result<(), DecodeError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }

    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let (head, tail) = self.buf.split_at_checked(n).ok_or(DecodeError::Truncated)?;
        self.buf = tail;
        Ok(head)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0; N];
        out.copy_from_slice(self.raw(N)?);
        Ok(out)
    }

    pub fn u32_le(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// A minimal LEB128 varint of at most 64 bits.
    pub fn uint(&mut self) -> Result<u64, DecodeError> {
        let mut v = 0u64;
        for i in 0..10 {
            let [byte] = self.array()?;
            let bits = u64::from(byte & 0x7f);
            if i == 9 && bits > 1 {
                return Err(DecodeError::BadVarint);
            }
            v |= bits << (7 * i);
            if byte & 0x80 == 0 {
                if i > 0 && byte == 0 {
                    return Err(DecodeError::BadVarint);
                }
                return Ok(v);
            }
        }
        Err(DecodeError::BadVarint)
    }

    pub fn int(&mut self) -> Result<i64, DecodeError> {
        Ok(unzigzag(self.uint()?))
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        u32::try_from(self.uint()?).map_err(|_| DecodeError::BadVarint)
    }

    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        u16::try_from(self.uint()?).map_err(|_| DecodeError::BadVarint)
    }

    pub fn i32(&mut self) -> Result<i32, DecodeError> {
        i32::try_from(self.int()?).map_err(|_| DecodeError::BadVarint)
    }

    /// A list length whose elements take at least `min_size` bytes each,
    /// checked against the bytes left so nothing is allocated beyond the
    /// data.
    pub fn count(&mut self, min_size: usize) -> Result<usize, DecodeError> {
        let n = self.uint()?;
        self.check_count(n, min_size)
    }

    /// Checks a list length read by the caller as in [`Self::count`].
    pub fn check_count(&self, n: u64, min_size: usize) -> Result<usize, DecodeError> {
        match usize::try_from(n) {
            Ok(len) if len <= self.remaining() / min_size => Ok(len),
            _ => Err(DecodeError::CountBeyondData(n)),
        }
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.count(1)?;
        self.raw(n)
    }

    pub fn string(&mut self) -> Result<String, DecodeError> {
        let bytes = self.bytes()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::{Reader, Writer, unzigzag, zigzag};
    use crate::DecodeError;

    #[test]
    fn zigzag_varints_round_trip_at_the_extremes() {
        let mut w = Writer::default();
        for v in [0, -1, 1, i64::MIN, i64::MAX] {
            w.int(v);
        }
        // Zigzag maps 0, -1, 1, MIN, MAX to 0, 1, 2, u64::MAX, u64::MAX - 1.
        assert_eq!(
            w.0,
            [
                0x00, 0x01, 0x02, //
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01, //
                0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
            ]
        );
        let mut r = Reader::new(&w.0);
        for v in [0, -1, 1, i64::MIN, i64::MAX] {
            assert_eq!(r.int(), Ok(v));
            assert_eq!(unzigzag(zigzag(v)), v);
        }
        assert_eq!(r.finish(), Ok(()));
    }

    #[test]
    fn overlong_and_oversized_varints_are_refused() {
        // 0 written in two bytes.
        assert_eq!(
            Reader::new(&[0x80, 0x00]).uint(),
            Err(DecodeError::BadVarint)
        );
        // 2^64: the tenth byte carries more than one bit.
        let too_big = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];
        assert_eq!(Reader::new(&too_big).uint(), Err(DecodeError::BadVarint));
        // Eleven bytes.
        let eleven = [0xff; 11];
        assert_eq!(Reader::new(&eleven).uint(), Err(DecodeError::BadVarint));
        assert_eq!(Reader::new(&[0x80]).uint(), Err(DecodeError::Truncated));
    }
}
