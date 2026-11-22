//! BIOS image identification.

use crate::bus::BIOS_SIZE;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BiosInfo {
    /// e.g. "4.1", when the ROM has a version string (all but v1.0).
    pub version: Option<String>,
    /// e.g. "12/16/97".
    pub date: Option<String>,
    pub region: Region,
    /// What a known non-console BIOS comes from, e.g. "PSP firmware 6.60".
    pub variant: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    NorthAmerica,
    Europe,
    Japan,
    /// Boots discs of every region (the PSP's PS1 emulator).
    Any,
    Unknown,
}

/// BIOS images that are not plain console ROMs, by FNV-1a fingerprint of
/// the whole image: their variant name and region.
const KNOWN_VARIANTS: &[(u64, &str, Region)] = &[
    // psxonpsp660.bin, SHA-1 96880d1ca92a016ff054be5159bb06fe03cb4e14: the PS1
    // emulator of PSP firmware 6.60. Based on 4.5 J, but region-free.
    (0x404f_abc1_7244_70ac, "PSP firmware 6.60", Region::Any),
];

/// FNV-1a: tells BIOS images apart, without a hashing dependency.
pub(crate) fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl Region {
    /// The letter of the licence string the CD-ROM controller reports.
    pub fn licence_letter(self) -> u8 {
        match self {
            Self::Europe => b'E',
            Self::Japan => b'I',
            // Region-free: the drive reports the disc's own region.
            Self::Any => 0,
            Self::NorthAmerica | Self::Unknown => b'A',
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NorthAmerica => "NTSC-U",
            Self::Europe => "PAL",
            Self::Japan => "NTSC-J",
            Self::Any => "any",
            Self::Unknown => "unknown",
        }
    }
}

/// Identifies a BIOS dump from its size and its version string
/// ("System ROM Version 4.1 12/16/97 A"). Returns `None` for anything that
/// does not look like a PlayStation BIOS.
pub fn identify(rom: &[u8]) -> Option<BiosInfo> {
    if rom.len() != BIOS_SIZE {
        return None;
    }
    // OpenBIOS: PCSX-Redux's open-source replacement, named at 0x78.
    if &rom[0x78..0x80] == b"OpenBIOS" {
        return Some(BiosInfo {
            version: None,
            date: None,
            region: Region::Any,
            variant: Some("OpenBIOS"),
        });
    }
    // Every retail BIOS has this at 0x108.
    if !rom[0x108..0x130].windows(4).any(|w| w == b"Sony") {
        return None;
    }
    const NEEDLE: &[u8] = b"System ROM Version ";
    // Some ROMs keep a stale "Version 1.0" string earlier on: use the last.
    let Some(at) = rom.windows(NEEDLE.len()).rposition(|w| w == NEEDLE) else {
        // v1.0 (SCPH-1000) is the only one without the string: Japanese.
        return Some(BiosInfo {
            version: Some("1.0".into()),
            date: None,
            region: Region::Japan,
            variant: None,
        });
    };
    let rest = &rom[at + NEEDLE.len()..];
    let end = rest.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(0).min(48);
    let text = String::from_utf8_lossy(&rest[..end]);
    let mut words = text.split_whitespace();
    let version = words.next().map(str::to_owned);
    let date = words.next().map(str::to_owned);
    let mut region = match words.next().and_then(|r| r.chars().next()) {
        Some('A') => Region::NorthAmerica,
        Some('E') => Region::Europe,
        Some('J') => Region::Japan,
        _ => Region::Unknown,
    };
    let fingerprint = fnv1a(rom);
    let variant = KNOWN_VARIANTS
        .iter()
        .find(|(f, ..)| *f == fingerprint)
        .map(|&(_, name, r)| {
            region = r;
            name
        });
    Some(BiosInfo {
        version,
        date,
        region,
        variant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_version_string() {
        let mut rom = vec![0u8; BIOS_SIZE];
        rom[0x108..0x10c].copy_from_slice(b"Sony");
        let s = b"System ROM Version 4.1 12/16/97 E\0";
        rom[0x7ff32..0x7ff32 + s.len()].copy_from_slice(s);
        let info = identify(&rom).unwrap();
        assert_eq!(info.version.as_deref(), Some("4.1"));
        assert_eq!(info.date.as_deref(), Some("12/16/97"));
        assert_eq!(info.region, Region::Europe);
        assert!(identify(&rom[..1024]).is_none());
    }
}
