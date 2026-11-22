//! BIOS image identification, from the ROM's file directory.
//!
//! A PS2 BIOS starts with a small file system: a directory of 16-byte
//! entries (name, extended-information size, size), found by its first
//! entry, `RESET`, and files laid out one after another from the start of
//! the ROM, each aligned to 16 bytes. The `ROMVER` file describes the ROM:
//! `0200EC20040614` is version 2.00, Europe, a console ROM (C), built on
//! 2004-06-14.

/// Size of a BIOS dump: 4 MiB for every model.
pub const BIOS_SIZE: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BiosInfo {
    /// e.g. "2.00".
    pub version: String,
    /// e.g. "2004-06-14".
    pub date: String,
    pub region: Region,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    NorthAmerica,
    Europe,
    Japan,
    Asia,
    China,
    Unknown,
}

impl Region {
    fn from_letter(letter: u8) -> Self {
        match letter {
            b'A' => Self::NorthAmerica,
            b'E' => Self::Europe,
            b'J' => Self::Japan,
            b'H' => Self::Asia,
            b'C' => Self::China,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NorthAmerica => "NTSC-U",
            Self::Europe => "PAL",
            Self::Japan => "NTSC-J",
            Self::Asia => "NTSC-Asia",
            Self::China => "NTSC-China",
            Self::Unknown => "unknown",
        }
    }
}

/// The directory of the ROM's file system: (name, offset, size) of each
/// file.
fn romdir(rom: &[u8]) -> Option<Vec<(&[u8], usize, usize)>> {
    const ENTRY: usize = 16;
    // The directory is in the first file, RESET, within its first 64 KiB.
    let start = (0..0x10000.min(rom.len()))
        .step_by(ENTRY)
        .find(|&at| rom.get(at..at + 6) == Some(b"RESET\0"))?;
    let mut files = Vec::new();
    let mut offset = 0usize;
    for entry in rom[start..].as_chunks::<ENTRY>().0.iter().take(256) {
        let name = &entry[..10];
        let len = name.iter().position(|&b| b == 0).unwrap_or(10);
        if len == 0 {
            return Some(files);
        }
        let size = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        files.push((&name[..len], offset, size));
        offset = offset.checked_add(size.div_ceil(16) * 16)?;
    }
    None
}

/// Identifies a BIOS dump from its size and its `ROMVER` file. Returns
/// `None` for anything that does not look like a PlayStation 2 BIOS.
pub fn identify(rom: &[u8]) -> Option<BiosInfo> {
    if rom.len() != BIOS_SIZE {
        return None;
    }
    let (_, offset, size) = romdir(rom)?.into_iter().find(|(name, ..)| *name == b"ROMVER")?;
    let text = rom.get(offset..offset + size.min(16))?;
    let text = std::str::from_utf8(text).ok()?.trim_end_matches(['\n', '\0']);
    // VVVV R T YYYYMMDD
    if text.len() < 14 || !text.is_char_boundary(4) || !text.is_ascii() {
        return None;
    }
    let (major, minor) = (&text[0..2], &text[2..4]);
    let date = &text[6..14];
    if !(major.bytes().chain(minor.bytes()).chain(date.bytes())).all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(BiosInfo {
        version: format!("{}.{minor}", major.trim_start_matches('0').max("0")),
        date: format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8]),
        region: Region::from_letter(text.as_bytes()[4]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ROM with a directory (RESET, ROMDIR, ROMVER) and a ROMVER file.
    fn rom(romver: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; BIOS_SIZE];
        let entry = |name: &[u8], size: u32| {
            let mut e = [0u8; 16];
            e[..name.len()].copy_from_slice(name);
            e[12..].copy_from_slice(&size.to_le_bytes());
            e
        };
        // RESET holds the directory, at 0x100: 0x200 bytes long.
        let dir = [
            entry(b"RESET", 0x200),
            entry(b"ROMDIR", 0x40),
            entry(b"ROMVER", romver.len() as u32),
        ];
        for (i, e) in dir.iter().enumerate() {
            rom[0x100 + i * 16..0x110 + i * 16].copy_from_slice(e);
        }
        rom[0x240..0x240 + romver.len()].copy_from_slice(romver);
        rom
    }

    #[test]
    fn reads_the_rom_version() {
        assert_eq!(
            identify(&rom(b"0200EC20040614\n\0")),
            Some(BiosInfo {
                version: "2.00".into(),
                date: "2004-06-14".into(),
                region: Region::Europe,
            })
        );
        assert_eq!(
            identify(&rom(b"0006AD20261105")).map(|b| b.version),
            Some("0.06".into())
        );
    }

    #[test]
    fn rejects_other_files() {
        assert_eq!(identify(&rom(b"not a version!!")), None);
        assert_eq!(identify(&vec![0u8; BIOS_SIZE]), None);
        assert_eq!(identify(&[0u8; 512 * 1024]), None);
    }
}
