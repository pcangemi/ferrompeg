//! Helpers for reading binary headers from a byte stream.

use std::io::{self, Read};

/// Fixed size integer reads on any reader. Containers mix endianness
/// (RIFF is little-endian, AIFF and FLAC big-endian), so both are here.
pub trait ReadBytes: Read {
    fn read_fixed<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut buf = [0u8; N];
        self.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        Ok(self.read_fixed::<1>()?[0])
    }

    fn read_u16_le(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.read_fixed()?))
    }

    fn read_u32_le(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.read_fixed()?))
    }

    fn read_u64_le(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.read_fixed()?))
    }

    fn read_u16_be(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.read_fixed()?))
    }

    fn read_u24_be(&mut self) -> io::Result<u32> {
        let b: [u8; 3] = self.read_fixed()?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    fn read_u32_be(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.read_fixed()?))
    }

    fn read_vec(&mut self, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        // `take` + `read_to_end` avoids allocating `len` bytes up front, so a
        // corrupt length field cannot make us reserve gigabytes.
        self.take(len as u64).read_to_end(&mut buf)?;
        if buf.len() < len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(buf)
    }

    fn skip(&mut self, len: u64) -> io::Result<()> {
        let copied = io::copy(&mut self.take(len), &mut io::sink())?;
        if copied < len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }

    /// Fills `buf` as far as the stream allows and returns the byte count,
    /// which is below `buf.len()` only at the end of the stream.
    fn read_full(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }
}

impl<R: Read + ?Sized> ReadBytes for R {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mixed_endianness() {
        let data = [1u8, 0, 0, 1, 0xAA, 0xBB, 0xCC, 9];
        let mut r = &data[..];
        assert_eq!(r.read_u16_le().unwrap(), 1);
        assert_eq!(r.read_u16_be().unwrap(), 1);
        assert_eq!(r.read_u24_be().unwrap(), 0xAABBCC);
        r.skip(1).unwrap();
        assert!(r.read_u8().is_err());
    }

    #[test]
    fn short_reads() {
        let data = [1u8, 2, 3];
        let mut r = &data[..];
        let mut buf = [0u8; 8];
        assert_eq!(r.read_full(&mut buf).unwrap(), 3);
        let mut r = &data[..];
        assert!(r.read_vec(4).is_err());
    }
}
