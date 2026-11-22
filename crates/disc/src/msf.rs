use crate::LEAD_IN_SECTORS;

/// A CD position in minutes, seconds and frames (75 frames per second).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Msf {
    pub m: u8,
    pub s: u8,
    pub f: u8,
}

impl Msf {
    /// The absolute position of an LBA, lead-in included.
    pub fn from_lba(lba: u32) -> Self {
        Self::from_sectors(lba + LEAD_IN_SECTORS)
    }

    /// A duration or relative position, without the lead-in.
    pub fn from_sectors(sectors: u32) -> Self {
        Self {
            m: (sectors / (75 * 60)).min(99) as u8,
            s: (sectors / 75 % 60) as u8,
            f: (sectors % 75) as u8,
        }
    }

    pub fn sectors(self) -> u32 {
        (u32::from(self.m) * 60 + u32::from(self.s)) * 75 + u32::from(self.f)
    }

    /// The LBA of an absolute position, saturating at 0 inside the lead-in.
    pub fn to_lba(self) -> u32 {
        self.sectors().saturating_sub(LEAD_IN_SECTORS)
    }

    pub fn from_bcd(m: u8, s: u8, f: u8) -> Self {
        Self {
            m: from_bcd(m),
            s: from_bcd(s),
            f: from_bcd(f),
        }
    }

    pub fn m_bcd(self) -> u8 {
        to_bcd(self.m)
    }

    pub fn s_bcd(self) -> u8 {
        to_bcd(self.s)
    }

    pub fn f_bcd(self) -> u8 {
        to_bcd(self.f)
    }
}

pub fn to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

pub fn from_bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0xf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let msf = Msf::from_lba(0);
        assert_eq!((msf.m, msf.s, msf.f), (0, 2, 0));
        assert_eq!(Msf::from_lba(12345).to_lba(), 12345);
        assert_eq!(Msf::from_bcd(0x12, 0x34, 0x56), Msf { m: 12, s: 34, f: 56 });
        assert_eq!(Msf { m: 12, s: 34, f: 56 }.f_bcd(), 0x56);
    }
}
