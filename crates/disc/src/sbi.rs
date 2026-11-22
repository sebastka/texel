//! SBI files: the subchannel Q data of the sectors a disc's protection
//! relies on, which CUE/BIN and CHD images do not keep.
//!
//! LibCrypt (European discs, from 1999) reads the position the drive
//! reports (GetlocP) on 32 sectors whose subchannel Q was corrupted when the
//! disc was pressed; a copy reports correct positions, and the game then
//! refuses to run, or hangs. Redump publishes these sectors as SBI files:
//!
//! ```text
//! "SBI\0", then per sector: absolute MSF (3 BCD bytes), a format byte
//! (1: the 10 bytes of Q follow, without CRC; 2: relative MSF only;
//! 3: absolute MSF only), and the data (10 or 3 bytes).
//! ```

use std::collections::HashMap;

use crate::Msf;

/// Subchannel Q without its CRC: control/ADR, track, index, relative MSF,
/// zero, absolute MSF (all BCD).
pub type SubQ = [u8; 10];

/// The sectors whose subchannel Q differs from what their position says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Subchannel {
    /// By LBA. Partial entries (formats 2 and 3) replace only their part.
    entries: HashMap<u32, Replacement>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Replacement {
    Full(SubQ),
    Relative([u8; 3]),
    Absolute([u8; 3]),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SbiError {
    NotSbi,
    Truncated,
    UnknownFormat(u8),
}

impl std::fmt::Display for SbiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSbi => f.write_str("not an SBI file"),
            Self::Truncated => f.write_str("the SBI file is truncated"),
            Self::UnknownFormat(t) => write!(f, "unknown SBI entry format {t}"),
        }
    }
}

impl std::error::Error for SbiError {}

impl Subchannel {
    pub fn parse_sbi(data: &[u8]) -> Result<Self, SbiError> {
        let mut rest = data.strip_prefix(b"SBI\0").ok_or(SbiError::NotSbi)?;
        let mut entries = HashMap::new();
        while !rest.is_empty() {
            let [m, s, f, format, tail @ ..] = rest else {
                return Err(SbiError::Truncated);
            };
            let lba = Msf::from_bcd(*m, *s, *f).to_lba();
            let len = match format {
                1 => 10,
                2 | 3 => 3,
                t => return Err(SbiError::UnknownFormat(*t)),
            };
            let bytes = tail.get(..len).ok_or(SbiError::Truncated)?;
            let replacement = match format {
                1 => Replacement::Full(bytes.try_into().expect("10 bytes")),
                2 => Replacement::Relative(bytes.try_into().expect("3 bytes")),
                _ => Replacement::Absolute(bytes.try_into().expect("3 bytes")),
            };
            entries.insert(lba, replacement);
            rest = &tail[len..];
        }
        Ok(Self { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The Q data of a sector: `normal` (from its position), with what the
    /// SBI file replaces.
    pub fn q(&self, lba: u32, mut normal: SubQ) -> SubQ {
        match self.entries.get(&lba) {
            Some(Replacement::Full(q)) => *q,
            Some(Replacement::Relative(msf)) => {
                normal[3..6].copy_from_slice(msf);
                normal
            }
            Some(Replacement::Absolute(msf)) => {
                normal[7..10].copy_from_slice(msf);
                normal
            }
            None => normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries() {
        // The first two sectors of Disney's Tarzan (Europe)'s SBI file.
        let mut sbi = b"SBI\0".to_vec();
        sbi.extend_from_slice(&[0x03, 0x09, 0x56, 1, 0x41, 1, 1, 0x03, 0x07, 0x56, 0, 0x03, 0x09, 0x56]);
        sbi.extend_from_slice(&[0x03, 0x09, 0x61, 1, 0x41, 1, 1, 0x03, 0x07, 0x65, 0, 0x03, 0x29, 0x61]);
        sbi.extend_from_slice(&[0x00, 0x02, 0x10, 3, 0x00, 0x02, 0x11]);
        let sub = Subchannel::parse_sbi(&sbi).unwrap();
        assert_eq!(sub.len(), 3);
        // 03:09:61 is LBA (3 * 60 + 9) * 75 + 61 - 150.
        let lba = (3 * 60 + 9) * 75 + 61 - 150;
        let normal = [0x41, 1, 1, 0x03, 0x07, 0x61, 0, 0x03, 0x09, 0x61];
        assert_eq!(sub.q(lba, normal), [0x41, 1, 1, 0x03, 0x07, 0x65, 0, 0x03, 0x29, 0x61]);
        assert_eq!(sub.q(lba + 1, normal), normal);
        // Format 3 replaces the absolute position only.
        assert_eq!(sub.q(10, [0; 10]), [0, 0, 0, 0, 0, 0, 0, 0x00, 0x02, 0x11]);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Subchannel::parse_sbi(b"PNG"), Err(SbiError::NotSbi));
        assert_eq!(
            Subchannel::parse_sbi(b"SBI\0\x03\x09\x56\x01\x41"),
            Err(SbiError::Truncated)
        );
        assert_eq!(
            Subchannel::parse_sbi(b"SBI\0\x03\x09\x56\x07"),
            Err(SbiError::UnknownFormat(7))
        );
    }
}
