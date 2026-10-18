//! Bit level reading and writing, most significant bit first.
//!
//! FLAC and the other bitstreams here pack fields from the high bit of each
//! byte down. GIF's LZW codes are the exception (least significant bit
//! first) and have their own packer in the GIF encoder.

use crate::error::{Error, Result};

pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    pub fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    pub fn is_aligned(&self) -> bool {
        self.pos % 8 == 0
    }

    /// Byte offset of the next unread byte, rounding a partial byte up.
    pub fn byte_position(&self) -> usize {
        self.pos.div_ceil(8)
    }

    pub fn bit_position(&self) -> usize {
        self.pos
    }

    fn eof() -> Error {
        Error::invalid_data("bitstream ended early")
    }

    pub fn read_bit(&mut self) -> Result<bool> {
        if self.pos >= self.data.len() * 8 {
            return Err(Self::eof());
        }
        let byte = self.data[self.pos / 8];
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(bit == 1)
    }

    /// Reads `n` bits (0 to 32) as an unsigned value.
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Ok(0);
        }
        if self.bits_left() < n as usize {
            return Err(Self::eof());
        }
        Ok(self.read_bits_unchecked(n) as u32)
    }

    /// Reads `n` bits (0 to 64) as an unsigned value.
    pub fn read_bits_u64(&mut self, n: u32) -> Result<u64> {
        debug_assert!(n <= 64);
        if n <= 32 {
            return self.read_bits(n).map(u64::from);
        }
        let high = self.read_bits(n - 32)? as u64;
        let low = self.read_bits(32)? as u64;
        Ok(high << 32 | low)
    }

    /// Caller guarantees `n <= 32` and enough bits are left.
    fn read_bits_unchecked(&mut self, n: u32) -> u64 {
        // Load up to 8 bytes covering the field into a big-endian window,
        // then shift the field down. Faster than bit by bit, and the window
        // always fits because a 32-bit field spans at most 5 bytes.
        let start = self.pos / 8;
        let mut window = 0u64;
        let end = (start + 8).min(self.data.len());
        for (i, &b) in self.data[start..end].iter().enumerate() {
            window |= (b as u64) << (56 - 8 * i);
        }
        let offset = (self.pos % 8) as u32;
        self.pos += n as usize;
        (window << offset) >> (64 - n)
    }

    /// Reads `n` bits as a two's complement signed value.
    pub fn read_bits_signed(&mut self, n: u32) -> Result<i32> {
        if n == 0 {
            return Ok(0);
        }
        let v = self.read_bits(n)?;
        let shift = 32 - n;
        Ok(((v << shift) as i32) >> shift)
    }

    /// Counts zero bits up to and including the next one bit, returning
    /// the number of zeros.
    pub fn read_unary(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        loop {
            if self.pos >= self.data.len() * 8 {
                return Err(Self::eof());
            }
            // Skip whole zero bytes when aligned: residuals with long runs
            // of zeros are common in quiet audio.
            if self.pos % 8 == 0 && self.data[self.pos / 8] == 0 {
                zeros += 8;
                self.pos += 8;
                continue;
            }
            if self.read_bit()? {
                return Ok(zeros);
            }
            zeros += 1;
        }
    }

    /// Reads a Rice coded signed value with parameter `k`: a unary quotient,
    /// `k` low bits, and a zigzag mapping back to a signed number.
    pub fn read_rice_signed(&mut self, k: u32) -> Result<i32> {
        let q = self.read_unary()?;
        let r = self.read_bits(k)?;
        let u = ((q as u64) << k) | r as u64;
        if u > u32::MAX as u64 {
            return Err(Error::invalid_data("rice value out of range"));
        }
        let u = u as u32;
        Ok(((u >> 1) as i32) ^ -((u & 1) as i32))
    }

    pub fn skip_bits(&mut self, n: usize) -> Result<()> {
        if self.bits_left() < n {
            return Err(Self::eof());
        }
        self.pos += n;
        Ok(())
    }

    /// Moves to the next byte boundary.
    pub fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }
}

#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Pending bits, right aligned.
    acc: u64,
    /// Number of pending bits in `acc` (always below 8 between calls).
    nbits: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(bytes: usize) -> Self {
        BitWriter {
            bytes: Vec::with_capacity(bytes),
            acc: 0,
            nbits: 0,
        }
    }

    /// Writes the low `n` bits (0 to 32) of `value`.
    pub fn write_bits(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        if n == 0 {
            return;
        }
        let mask = if n == 32 { u32::MAX } else { (1 << n) - 1 };
        self.acc = (self.acc << n) | (value & mask) as u64;
        self.nbits += n;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.bytes.push((self.acc >> self.nbits) as u8);
        }
        self.acc &= (1u64 << self.nbits) - 1;
    }

    /// Writes the low `n` bits (0 to 64) of `value`.
    pub fn write_bits_u64(&mut self, value: u64, n: u32) {
        if n > 32 {
            self.write_bits((value >> 32) as u32, n - 32);
            self.write_bits(value as u32, 32);
        } else {
            self.write_bits(value as u32, n);
        }
    }

    pub fn write_bit(&mut self, bit: bool) {
        self.write_bits(bit as u32, 1);
    }

    /// Writes a signed value in `n` bits, two's complement.
    pub fn write_bits_signed(&mut self, value: i32, n: u32) {
        self.write_bits(value as u32, n);
    }

    /// Writes `zeros` zero bits followed by a one bit.
    pub fn write_unary(&mut self, mut zeros: u32) {
        while zeros >= 32 {
            self.write_bits(0, 32);
            zeros -= 32;
        }
        self.write_bits(1, zeros + 1);
    }

    /// Writes a signed value Rice coded with parameter `k`, the inverse of
    /// [`BitReader::read_rice_signed`].
    pub fn write_rice_signed(&mut self, value: i32, k: u32) {
        // Zigzag in 64 bits: `value << 1` would overflow an i32 for
        // magnitudes of 2^30 and above.
        let v = value as i64;
        let u = ((v << 1) ^ (v >> 63)) as u32;
        self.write_unary(u >> k);
        if k > 0 {
            self.write_bits(u, k);
        }
    }

    pub fn is_aligned(&self) -> bool {
        self.nbits == 0
    }

    /// Number of bits written so far.
    pub fn bit_len(&self) -> usize {
        self.bytes.len() * 8 + self.nbits as usize
    }

    /// Pads with zero bits up to the next byte boundary.
    pub fn align(&mut self) {
        if self.nbits > 0 {
            self.write_bits(0, 8 - self.nbits);
        }
    }

    /// Bytes completed so far (excludes a partial trailing byte).
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Pads to a byte boundary and returns the buffer.
    pub fn into_bytes(mut self) -> Vec<u8> {
        self.align();
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fields_across_bytes() {
        let data = [0b1010_1100, 0b0101_0011, 0xFF];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits(3).unwrap(), 0b101);
        assert_eq!(r.read_bits(7).unwrap(), 0b0110001);
        assert!(!r.read_bit().unwrap());
        assert_eq!(r.read_bits(13).unwrap(), 0b1_0011_1111_1111);
        assert!(r.read_bit().is_err());
    }

    #[test]
    fn signed_and_unary() {
        let mut w = BitWriter::new();
        w.write_bits_signed(-3, 5);
        w.write_unary(0);
        w.write_unary(12);
        w.write_bits(0x7, 3);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits_signed(5).unwrap(), -3);
        assert_eq!(r.read_unary().unwrap(), 0);
        assert_eq!(r.read_unary().unwrap(), 12);
        assert_eq!(r.read_bits(3).unwrap(), 7);
    }

    #[test]
    fn rice_round_trip() {
        let values = [0, 1, -1, 2, -2, 100, -100, 32767, -32768, 1 << 20];
        for k in [0, 1, 4, 10, 14] {
            let mut w = BitWriter::new();
            for &v in &values {
                w.write_rice_signed(v, k);
            }
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            for &v in &values {
                assert_eq!(r.read_rice_signed(k).unwrap(), v, "k={k}");
            }
        }
    }

    #[test]
    fn rice_extremes_do_not_overflow() {
        let mut w = BitWriter::new();
        for v in [i32::MAX, i32::MIN, 1 << 30, -(1 << 30)] {
            w.write_rice_signed(v, 30);
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for v in [i32::MAX, i32::MIN, 1 << 30, -(1 << 30)] {
            assert_eq!(r.read_rice_signed(30).unwrap(), v);
        }
    }

    #[test]
    fn wide_fields() {
        let mut w = BitWriter::new();
        w.write_bit(true);
        w.write_bits(u32::MAX, 32);
        w.write_bits_u64(0x1_2345_6789, 36);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert!(r.read_bit().unwrap());
        assert_eq!(r.read_bits(32).unwrap(), u32::MAX);
        assert_eq!(r.read_bits_u64(36).unwrap(), 0x1_2345_6789);
    }

    #[test]
    fn alignment() {
        let mut w = BitWriter::new();
        w.write_bits(1, 3);
        assert!(!w.is_aligned());
        w.align();
        assert_eq!(w.bit_len(), 8);
        let mut r = BitReader::new(&[0xFF, 0x80]);
        r.read_bits(3).unwrap();
        r.align();
        assert_eq!(r.byte_position(), 1);
        assert!(r.read_bit().unwrap());
    }
}
