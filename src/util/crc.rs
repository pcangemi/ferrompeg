//! Table driven CRCs used by the containers.
//!
//! FLAC uses CRC-8 (polynomial 0x07) on frame headers and CRC-16
//! (polynomial 0x8005) on whole frames, both unreflected with a zero
//! initial value. Tables are built at compile time.

const fn crc8_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const fn crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC8: [u8; 256] = crc8_table();
static CRC16: [u16; 256] = crc16_table();

pub fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |crc, &b| CRC8[(crc ^ b) as usize])
}

pub fn crc16(data: &[u8]) -> u16 {
    crc16_update(0, data)
}

/// Continues a CRC-16 over more data, so frames can be checked in pieces.
pub fn crc16_update(crc: u16, data: &[u8]) -> u16 {
    data.iter().fold(crc, |crc, &b| {
        (crc << 8) ^ CRC16[((crc >> 8) as u8 ^ b) as usize]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Check values from the CRC catalogue for the input "123456789":
    // CRC-8/SMBUS is 0xF4 and CRC-16/UMTS (a.k.a. BUYPASS) is 0xFEE8.
    #[test]
    fn catalogue_check_values() {
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(crc16(b"123456789"), 0xFEE8);
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data = b"the quick brown fox";
        let partial = crc16_update(crc16(&data[..7]), &data[7..]);
        assert_eq!(partial, crc16(data));
    }
}
